//! Liveness-based register allocation for value locals — collapses the per-value
//! `v<raw>` locals down to the few simultaneously live, so a function stays under
//! luajit's 200-locals-per-function PARSE cap (a hard error that, since the whole
//! program is one chunk, would kill the entire load).
//!
//! Each value (`Idx<Inst>` that produces a value) is defined exactly once (the
//! IR is SSA-like for values; mutation rides on cells), so liveness is a standard
//! backward dataflow fixpoint over the CFG; non-interfering values then share a
//! color via greedy graph coloring. Each color is one Lua local `r<color>`.
//!
//! Cells are NOT colored here (they are mutable, comparatively few); they keep
//! their `c<raw>` locals. Correctness-first and simple — not the old backend's
//! perf-tuned two-pool scheme; this runs once per emit and the corpus functions
//! are small.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::{Idx, RawIdx};

use crate::{CountedLoop, collect_lambda_params, successors};
use skull_mir::mir::{Block, Cell, Inst, MirFunc, Term};

/// A dense fixed-size bit set over `0..n` — used for cell liveness/interference,
/// where set union/diff and "interfere with the whole live set" are word-parallel
/// (`O(n/64)`) instead of the per-element hashing a `HashSet` pays. The cell
/// coloring pass is the skull compile hot spot on inlined functions.
#[derive(Clone)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    fn new(n: usize) -> Self {
        BitSet {
            words: vec![0; n.div_ceil(64)],
        }
    }
    fn set(&mut self, i: usize) {
        self.words[i >> 6] |= 1u64 << (i & 63);
    }
    fn remove(&mut self, i: usize) {
        self.words[i >> 6] &= !(1u64 << (i & 63));
    }
    fn get(&self, i: usize) -> bool {
        self.words[i >> 6] & (1u64 << (i & 63)) != 0
    }
    /// Zero every bit (reuse as a scratch buffer without reallocating).
    fn clear(&mut self) {
        self.words.iter_mut().for_each(|w| *w = 0);
    }
    /// `self |= o`; returns whether any bit was newly set.
    fn or_with(&mut self, o: &[u64]) -> bool {
        let mut changed = false;
        for (a, b) in self.words.iter_mut().zip(o.iter()) {
            let merged = *a | *b;
            changed |= merged != *a;
            *a = merged;
        }
        changed
    }
}

/// `rows` bit sets of one width in one allocation: the per-block liveness
/// tables. A `Vec<BitSet>` paid an allocation per block per table.
#[derive(Clone)]
struct BitRows {
    width: usize,
    words: Vec<u64>,
}

impl BitRows {
    fn new(rows: usize, n: usize) -> Self {
        let width = n.div_ceil(64);
        BitRows {
            width,
            words: vec![0; rows * width],
        }
    }
    fn row(&self, r: usize) -> &[u64] {
        &self.words[r * self.width..(r + 1) * self.width]
    }
    fn row_mut(&mut self, r: usize) -> &mut [u64] {
        &mut self.words[r * self.width..(r + 1) * self.width]
    }
    fn set(&mut self, r: usize, i: usize) {
        self.words[r * self.width + (i >> 6)] |= 1u64 << (i & 63);
    }
    fn get(&self, r: usize, i: usize) -> bool {
        self.words[r * self.width + (i >> 6)] & (1u64 << (i & 63)) != 0
    }
    fn for_each(&self, r: usize, f: impl FnMut(usize)) {
        for_each_bit(self.row(r), f);
    }
}

fn for_each_bit(words: &[u64], mut f: impl FnMut(usize)) {
    for (wi, &w) in words.iter().enumerate() {
        let mut bits = w;
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            f(wi * 64 + b);
            bits &= bits - 1;
        }
    }
}

/// The blocks one body emits, in emit order, with the edges between them as
/// indices into that order. Every liveness pass over the body walks it.
pub(crate) struct Flow {
    /// `raw(block) -> index in the order`; `u32::MAX` for a block outside it.
    pos: Vec<u32>,
    succ_off: Vec<u32>,
    succ: Vec<u32>,
    /// One entry longer than `succ_off`: the fill cursor trails by a slot.
    pred_off: Vec<u32>,
    pred: Vec<u32>,
}

impl Flow {
    pub(crate) fn new(func: &MirFunc, order: &[Idx<Block>]) -> Self {
        let n = order.len();
        let mut pos = vec![u32::MAX; func.blocks.len()];
        for (i, &b) in order.iter().enumerate() {
            pos[raw(b)] = i as u32;
        }
        let mut succ_off = Vec::with_capacity(n + 1);
        let mut succ = Vec::with_capacity(2 * n);
        // Counted two slots up, so that after the prefix sum slot `i + 1` is
        // the start of block `i` and filling it leaves the start of `i + 1`.
        let mut pred_off = vec![0u32; n + 2];
        for &b in order {
            succ_off.push(succ.len() as u32);
            successors(&func.blocks[b].term, |s| {
                let si = pos[raw(s)];
                if si != u32::MAX {
                    succ.push(si);
                    pred_off[si as usize + 2] += 1;
                }
            });
        }
        succ_off.push(succ.len() as u32);
        for i in 2..n + 2 {
            pred_off[i] += pred_off[i - 1];
        }
        let mut pred = vec![0u32; succ.len()];
        for bi in 0..n {
            for &s in &succ[succ_off[bi] as usize..succ_off[bi + 1] as usize] {
                let at = &mut pred_off[s as usize + 1];
                pred[*at as usize] = bi as u32;
                *at += 1;
            }
        }
        Flow {
            pos,
            succ_off,
            succ,
            pred_off,
            pred,
        }
    }

    fn len(&self) -> usize {
        self.succ_off.len() - 1
    }

    /// The index of `b` in the order, `None` for a block outside the body.
    fn index(&self, b: Idx<Block>) -> Option<usize> {
        match self.pos.get(raw(b)) {
            Some(&i) if i != u32::MAX => Some(i as usize),
            _ => None,
        }
    }

    fn succs(&self, bi: usize) -> &[u32] {
        &self.succ[self.succ_off[bi] as usize..self.succ_off[bi + 1] as usize]
    }

    fn preds(&self, bi: usize) -> &[u32] {
        &self.pred[self.pred_off[bi] as usize..self.pred_off[bi + 1] as usize]
    }

    /// The bits live into each block: the least solution of
    /// `in = use | (out & !def)`, where a block's `out` is the union of its
    /// successors' `in`. A block is revisited only when a successor's set
    /// grew, so the work follows the propagation instead of sweeping the body
    /// until nothing changes.
    fn live_in(&self, use_set: &BitRows, def_set: &BitRows) -> BitRows {
        let n = self.len();
        let width = use_set.width;
        let mut live = BitRows {
            width,
            words: vec![0; n * width],
        };
        if width == 0 {
            return live;
        }
        // Popped from the end, so the first sweep runs the order backward.
        let mut work: Vec<u32> = (0..n as u32).collect();
        let mut queued = vec![true; n];
        while let Some(bi) = work.pop() {
            let bi = bi as usize;
            queued[bi] = false;
            let mut grew = false;
            for wi in 0..width {
                let mut out = 0u64;
                for &s in self.succs(bi) {
                    out |= live.words[s as usize * width + wi];
                }
                let at = bi * width + wi;
                let merged = live.words[at] | use_set.words[at] | (out & !def_set.words[at]);
                grew |= merged != live.words[at];
                live.words[at] = merged;
            }
            if grew {
                for &p in self.preds(bi) {
                    if !queued[p as usize] {
                        queued[p as usize] = true;
                        work.push(p);
                    }
                }
            }
        }
        live
    }

    /// The bits live out of block `bi`, given the bits live into every block.
    fn live_out(&self, live_in: &BitRows, bi: usize, out: &mut BitSet) {
        out.clear();
        for &s in self.succs(bi) {
            out.or_with(live_in.row(s as usize));
        }
    }
}

/// Sparse symmetric adjacency in CSR form, built from an undirected edge list:
/// the large-body form of [`Interference`]. Each input edge `(a, b)` is stored
/// in BOTH directions, so `neighbors(i)` yields every name interfering with `i`
/// regardless of discovery direction.
struct Csr {
    off: Vec<u32>,
    adj: Vec<u32>,
}

impl Csr {
    fn from_edges(edges: &[(u32, u32)], n: usize) -> Self {
        let mut deg = vec![0u32; n];
        for &(a, b) in edges {
            deg[a as usize] += 1;
            deg[b as usize] += 1;
        }
        let mut off = vec![0u32; n + 1];
        for i in 0..n {
            off[i + 1] = off[i] + deg[i];
        }
        let mut adj = vec![0u32; off[n] as usize];
        let mut fill = off.clone();
        for &(a, b) in edges {
            adj[fill[a as usize] as usize] = b;
            fill[a as usize] += 1;
            adj[fill[b as usize] as usize] = a;
            fill[b as usize] += 1;
        }
        Csr { off, adj }
    }

    fn neighbors(&self, i: usize) -> &[u32] {
        &self.adj[self.off[i] as usize..self.off[i + 1] as usize]
    }
}

/// Symmetric interference between `n` names. Up to [`DENSE_NAMES`] names it is
/// a bit matrix, where a def interferes with its whole live set by OR-ing
/// words into its row; above that, a sparse edge list in CSR form, since a
/// matrix's zeroing and row scans grow with the square of the name count.
enum Interference {
    Dense(BitRows),
    Sparse(Vec<(u32, u32)>),
}

const DENSE_NAMES: usize = 1024;

impl Interference {
    fn new(n: usize) -> Self {
        if n <= DENSE_NAMES {
            Interference::Dense(BitRows::new(n, n))
        } else {
            Interference::Sparse(Vec::new())
        }
    }

    /// `d` interferes with every name in `live` that `allowed` admits (all
    /// names when `None`), except itself.
    fn add_live(&mut self, d: usize, live: &[u64], allowed: Option<&[u64]>) {
        match self {
            Interference::Dense(rows) => {
                let width = rows.width;
                for wi in 0..width {
                    let mut w = live[wi];
                    if let Some(allowed) = allowed {
                        w &= allowed[wi];
                    }
                    if wi == d >> 6 {
                        w &= !(1u64 << (d & 63));
                    }
                    if w == 0 {
                        continue;
                    }
                    rows.words[d * width + wi] |= w;
                    while w != 0 {
                        let o = wi * 64 + w.trailing_zeros() as usize;
                        rows.set(o, d);
                        w &= w - 1;
                    }
                }
            }
            Interference::Sparse(edges) => {
                for_each_bit(live, |o| {
                    if o != d && allowed.is_none_or(|a| a[o >> 6] & (1u64 << (o & 63)) != 0) {
                        edges.push((d as u32, o as u32));
                    }
                });
            }
        }
    }

    fn add(&mut self, a: usize, b: usize) {
        match self {
            Interference::Dense(rows) => {
                rows.set(a, b);
                rows.set(b, a);
            }
            Interference::Sparse(edges) => edges.push((a as u32, b as u32)),
        }
    }

    fn finish(self, n: usize) -> Neighbors {
        match self {
            Interference::Dense(rows) => Neighbors::Dense(rows),
            Interference::Sparse(edges) => Neighbors::Sparse(Csr::from_edges(&edges, n)),
        }
    }
}

enum Neighbors {
    Dense(BitRows),
    Sparse(Csr),
}

impl Neighbors {
    fn for_each(&self, i: usize, mut f: impl FnMut(usize)) {
        match self {
            Neighbors::Dense(rows) => rows.for_each(i, f),
            Neighbors::Sparse(csr) => csr.neighbors(i).iter().for_each(|&o| f(o as usize)),
        }
    }
}

/// Raw arena index as a `usize` — the dense key for the `Vec` side-tables that
/// replace `Idx`-keyed `HashMap`s in the hot liveness/interference loops (an `Idx`
/// is dense over its arena, so a flat array beats any hash).
#[inline]
fn raw<T>(i: Idx<T>) -> usize {
    u32::from(i.into_raw()) as usize
}

/// Value operands an instruction READS (not the value it defines).
pub(crate) fn inst_uses(inst: &Inst, push: &mut impl FnMut(Idx<Inst>)) {
    inst.uses_each(push);
}

/// Value operands a terminator reads, seeing THROUGH inlined operands: if an
/// operand is tree-inlined (emitted at this use site, not as a register), its own
/// effective uses are yielded recursively so the colorer keeps the registers the
/// inlined expression reads live up to here. Non-inlined operands yield as-is.
pub(crate) fn effective_uses(
    func: &MirFunc,
    inline: &[bool],
    inst: &Inst,
    push: &mut impl FnMut(Idx<Inst>),
) {
    inst_uses(inst, &mut |u| {
        if inline[raw(u)] {
            effective_uses(func, inline, &func.insts[u], push);
        } else {
            push(u);
        }
    });
}

/// Value operands a terminator reads.
pub(crate) fn term_uses(term: &Term, push: &mut impl FnMut(Idx<Inst>)) {
    term.uses_each(push);
}

/// A counted loop the emitter renders as a Lua `for ... do .. end` block:
/// header + body region (excluding header and latch), as recognized by the
/// emitter's `recognize_counted_loops`. Used to partition value colors by
/// lexical scope.
pub(crate) struct LoopScope {
    pub header: Idx<Block>,
    pub region: Vec<Idx<Block>>,
}

/// Backward liveness over a small set of tracked bits (some values, some
/// cells) in one body, with each emitted statement's and terminator's reads
/// and writes of them flattened once. The two fold analyses below walk it
/// once more, statement by statement, to find their conflicts.
struct TrackedLiveness<'a> {
    flow: &'a Flow,
    /// Per block: `(first, end)` into `stmts`.
    block_stmts: Vec<(u32, u32)>,
    stmts: Vec<Idx<Inst>>,
    /// Per statement, then per block terminator: `(uses, defs)` ranges.
    stmt_bits: Vec<(u32, u32, u32)>,
    term_bits: Vec<(u32, u32)>,
    bits: Vec<u32>,
    live_in: BitRows,
    width: usize,
}

impl<'a> TrackedLiveness<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        func: &MirFunc,
        scope: &[Idx<Block>],
        flow: &'a Flow,
        inline: &[bool],
        width: usize,
        uses: impl Fn(Idx<Inst>, &mut Vec<u32>),
        defs: impl Fn(Idx<Inst>, &mut Vec<u32>),
        term: impl Fn(Idx<Block>, &mut Vec<u32>),
    ) -> Self {
        let n = scope.len();
        let mut t = TrackedLiveness {
            flow,
            block_stmts: Vec::with_capacity(n),
            stmts: Vec::new(),
            stmt_bits: Vec::new(),
            term_bits: Vec::with_capacity(n),
            bits: Vec::new(),
            live_in: BitRows::new(0, width),
            width,
        };
        let mut use_set = BitRows::new(n, width);
        let mut def_set = BitRows::new(n, width);
        for (bi, &b) in scope.iter().enumerate() {
            let first = t.stmts.len() as u32;
            for &id in &func.blocks[b].insts {
                if inline[raw(id)] {
                    continue;
                }
                let u0 = t.bits.len() as u32;
                uses(id, &mut t.bits);
                let d0 = t.bits.len() as u32;
                defs(id, &mut t.bits);
                let d1 = t.bits.len() as u32;
                for &k in &t.bits[u0 as usize..d0 as usize] {
                    if !def_set.get(bi, k as usize) {
                        use_set.set(bi, k as usize);
                    }
                }
                for &k in &t.bits[d0 as usize..d1 as usize] {
                    def_set.set(bi, k as usize);
                }
                t.stmts.push(id);
                t.stmt_bits.push((u0, d0, d1));
            }
            t.block_stmts.push((first, t.stmts.len() as u32));
            let u0 = t.bits.len() as u32;
            term(b, &mut t.bits);
            for &k in &t.bits[u0 as usize..] {
                if !def_set.get(bi, k as usize) {
                    use_set.set(bi, k as usize);
                }
            }
            t.term_bits.push((u0, t.bits.len() as u32));
        }
        t.live_in = flow.live_in(&use_set, &def_set);
        t
    }

    /// Visit every statement backward with the bits live just after it.
    fn walk(&self, mut at: impl FnMut(Idx<Inst>, &BitSet)) {
        let mut live = BitSet::new(self.width);
        for bi in 0..self.block_stmts.len() {
            self.flow.live_out(&self.live_in, bi, &mut live);
            let (t0, t1) = self.term_bits[bi];
            for &k in &self.bits[t0 as usize..t1 as usize] {
                live.set(k as usize);
            }
            let (first, end) = self.block_stmts[bi];
            for si in (first..end).rev() {
                let id = self.stmts[si as usize];
                at(id, &live);
                let (u0, d0, d1) = self.stmt_bits[si as usize];
                for &k in &self.bits[d0 as usize..d1 as usize] {
                    live.remove(k as usize);
                }
                for &k in &self.bits[u0 as usize..d0 as usize] {
                    live.set(k as usize);
                }
            }
        }
    }
}

/// What each emitted statement and terminator of one body reads where it is
/// printed, in operand order: an unfolded value, or the cell of a folded load,
/// seeing through every other folded operand. A statement's own cell reads
/// come first. Seeing through folded operands is the expensive part of every
/// liveness pass after folding, so it is flattened once; the later passes
/// fold more (never less), and [`Reads::values`] and [`Reads::cells`] expand
/// a leaf value they fold where it is read.
pub(crate) struct Reads {
    /// By raw instruction, then by raw block for terminators: a range of
    /// `leaves`.
    stmt: Vec<(u32, u32)>,
    term: Vec<(u32, u32)>,
    /// A value's raw index, or a cell's with [`CELL_LEAF`] set.
    leaves: Vec<u32>,
}

const CELL_LEAF: u32 = 1 << 31;

impl Reads {
    pub(crate) fn new(func: &MirFunc, blocks: &[Idx<Block>], inline: &[bool]) -> Self {
        fn operand(func: &MirFunc, inline: &[bool], v: Idx<Inst>, leaves: &mut Vec<u32>) {
            if !inline[raw(v)] {
                leaves.push(raw(v) as u32);
                return;
            }
            match &func.insts[v] {
                Inst::Load(c) => leaves.push(raw(*c) as u32 | CELL_LEAF),
                nested => inst_uses(nested, &mut |u| operand(func, inline, u, leaves)),
            }
        }
        let mut reads = Reads {
            stmt: vec![(0, 0); func.insts.len()],
            term: vec![(0, 0); func.blocks.len()],
            leaves: Vec::new(),
        };
        for &b in blocks {
            let block = &func.blocks[b];
            for &id in &block.insts {
                if inline[raw(id)] {
                    continue;
                }
                let start = reads.leaves.len() as u32;
                let inst = &func.insts[id];
                cell_uses(inst, &mut |c| reads.leaves.push(raw(c) as u32 | CELL_LEAF));
                inst_uses(inst, &mut |u| operand(func, inline, u, &mut reads.leaves));
                reads.stmt[raw(id)] = (start, reads.leaves.len() as u32);
            }
            let start = reads.leaves.len() as u32;
            term_uses(&block.term, &mut |u| {
                operand(func, inline, u, &mut reads.leaves)
            });
            reads.term[raw(b)] = (start, reads.leaves.len() as u32);
        }
        reads
    }

    fn stmt(&self, id: Idx<Inst>) -> &[u32] {
        let (start, end) = self.stmt[raw(id)];
        &self.leaves[start as usize..end as usize]
    }

    fn term(&self, b: Idx<Block>) -> &[u32] {
        let (start, end) = self.term[raw(b)];
        &self.leaves[start as usize..end as usize]
    }

    /// The registers `leaves` read under `inline`, as [`effective_uses`].
    fn values(func: &MirFunc, inline: &[bool], leaves: &[u32], mut push: impl FnMut(Idx<Inst>)) {
        for &leaf in leaves {
            if leaf & CELL_LEAF != 0 {
                continue;
            }
            let v = Idx::from_raw(RawIdx::from_u32(leaf));
            if inline[raw(v)] {
                effective_uses(func, inline, &func.insts[v], &mut push);
            } else {
                push(v);
            }
        }
    }

    /// The cells `leaves` read under `inline`, counting a value computed in a
    /// cell, as a statement's own cell reads plus the folded loads among its operands.
    fn cells(
        func: &MirFunc,
        inline: &[bool],
        alias: &[Option<Idx<Cell>>],
        leaves: &[u32],
        mut push: impl FnMut(Idx<Cell>),
    ) {
        for &leaf in leaves {
            if leaf & CELL_LEAF != 0 {
                push(Idx::from_raw(RawIdx::from_u32(leaf & !CELL_LEAF)));
                continue;
            }
            let v = Idx::from_raw(RawIdx::from_u32(leaf));
            if let Some(cell) = alias_at(alias, v) {
                push(cell);
            } else if inline[raw(v)] {
                match &func.insts[v] {
                    Inst::Load(cell) => push(*cell),
                    nested => inlined_operand_cell_uses(func, inline, alias, nested, &mut push),
                }
            }
        }
    }
}

/// The loads in `scope` that can render as their cell at every use: no
/// statement writes the cell while the loaded value is live. Folding one
/// removes its register, the copy into it and that copy's kill; the cell
/// stays live to the last use instead. `pinned` cells are excluded.
pub(crate) fn stable_loads(
    func: &MirFunc,
    scope: &[Idx<Block>],
    flow: &Flow,
    inline: &[bool],
    pinned: &[bool],
    loops: &HashMap<Idx<Block>, CountedLoop>,
    reads: &Reads,
) -> Vec<Idx<Inst>> {
    let mut written = vec![false; func.cells.len()];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            func.insts[id].cell_writes_each(&mut |c| written[raw(c)] = true);
        }
    }
    // A load of a cell nothing in the body writes is stable outright.
    let mut stable = Vec::new();
    let mut li = vec![u32::MAX; func.insts.len()];
    let mut loads: Vec<(Idx<Inst>, Idx<Cell>)> = Vec::new();
    for &b in scope {
        for &id in &func.blocks[b].insts {
            if let Inst::Load(cell) = func.insts[id]
                && !inline[raw(id)]
                && !pinned[raw(cell)]
            {
                if written[raw(cell)] {
                    li[raw(id)] = loads.len() as u32;
                    loads.push((id, cell));
                } else {
                    stable.push(id);
                }
            }
        }
    }
    if loads.is_empty() {
        return stable;
    }
    let mut by_cell = vec![Vec::new(); func.cells.len()];
    for (k, &(_, cell)) in loads.iter().enumerate() {
        by_cell[raw(cell)].push(k as u32);
    }
    let tracked = |v: Idx<Inst>, bits: &mut Vec<u32>| {
        if li[raw(v)] != u32::MAX {
            bits.push(li[raw(v)]);
        }
    };
    let liveness = TrackedLiveness::new(
        func,
        scope,
        flow,
        inline,
        loads.len(),
        |id, bits| Reads::values(func, inline, reads.stmt(id), |v| tracked(v, bits)),
        |id, bits| tracked(id, bits),
        |b, bits| Reads::values(func, inline, reads.term(b), |v| tracked(v, bits)),
    );
    // A statement writes its cells after it reads its operands, so a write
    // conflicts with a load that is still live after the statement.
    let mut conflict = vec![false; loads.len()];
    // A counted loop's counter is a fresh Lua local inside the `for`; outside
    // it the cell's name is the outer local, so a load still live after an
    // early exit must stay in a register.
    for (&header, cl) in loops {
        for &k in &by_cell[raw(cl.slot)] {
            conflict[k as usize] = scope.iter().enumerate().any(|(bi, &b)| {
                b != header
                    && b != cl.latch
                    && !cl.region.contains(&b)
                    && liveness.live_in.get(bi, k as usize)
            });
        }
    }
    liveness.walk(|id, live| {
        func.insts[id].cell_writes_each(&mut |cell| {
            for &k in &by_cell[raw(cell)] {
                conflict[k as usize] |= live.get(k as usize);
            }
        });
    });
    stable.extend(
        loads
            .iter()
            .zip(conflict)
            .filter(|&(_, c)| !c)
            .map(|(&(id, _), _)| id),
    );
    stable
}

/// Values to compute directly in the cell a later `Store` copies them into,
/// as a dense `raw(inst) -> cell` table (empty when there are none). A value
/// qualifies when its cell is dead where the value is defined and no other
/// statement writes the cell while the value is live, so the cell holds the
/// value at every use; the store then assigns the cell to itself. Two values
/// of one cell never overlap. `eligible` says which values may move; `pinned`
/// cells never take one.
pub(crate) fn store_targets(
    func: &MirFunc,
    scope: &[Idx<Block>],
    flow: &Flow,
    inline: &[bool],
    pinned: &[bool],
    reads: &Reads,
    eligible: impl Fn(Idx<Inst>, Idx<Cell>) -> bool,
) -> Vec<Option<Idx<Cell>>> {
    // Candidate k: value `vals[k]` into cell `cells[k]`.
    let mut vi = vec![u32::MAX; func.insts.len()];
    let mut vals: Vec<Idx<Inst>> = Vec::new();
    let mut cells: Vec<Idx<Cell>> = Vec::new();
    let mut defined = vec![false; func.insts.len()];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            defined[raw(id)] = true;
            if let Inst::Store { cell, value } = func.insts[id]
                && !inline[raw(id)]
                && !inline[raw(value)]
                // Defined earlier in this scope, as a dominating def must be.
                && defined[raw(value)]
                && vi[raw(value)] == u32::MAX
                && !pinned[raw(cell)]
                && func.insts[value].produces_value()
                && eligible(value, cell)
            {
                vi[raw(value)] = vals.len() as u32;
                vals.push(value);
                cells.push(cell);
            }
        }
    }
    if vals.is_empty() {
        return Vec::new();
    }
    let k_count = vals.len();
    // Bits: the candidate values in `0..k_count`, then their cells.
    let mut ci = vec![u32::MAX; func.cells.len()];
    let mut c_count = 0u32;
    for &c in &cells {
        if ci[raw(c)] == u32::MAX {
            ci[raw(c)] = k_count as u32 + c_count;
            c_count += 1;
        }
    }
    let mut by_cell = vec![Vec::new(); c_count as usize];
    for (k, &c) in cells.iter().enumerate() {
        by_cell[(ci[raw(c)] as usize) - k_count].push(k as u32);
    }
    let value_bit = |v: Idx<Inst>, bits: &mut Vec<u32>| {
        if vi[raw(v)] != u32::MAX {
            bits.push(vi[raw(v)]);
        }
    };
    let cell_bit = |c: Idx<Cell>, bits: &mut Vec<u32>| {
        if ci[raw(c)] != u32::MAX {
            bits.push(ci[raw(c)]);
        }
    };
    let liveness = TrackedLiveness::new(
        func,
        scope,
        flow,
        inline,
        k_count + c_count as usize,
        |id, bits| {
            Reads::values(func, inline, reads.stmt(id), |v| value_bit(v, bits));
            Reads::cells(func, inline, &[], reads.stmt(id), |c| cell_bit(c, bits));
        },
        |id, bits| {
            value_bit(id, bits);
            func.insts[id].cell_writes_each(&mut |c| cell_bit(c, bits));
        },
        |b, bits| {
            Reads::values(func, inline, reads.term(b), |v| value_bit(v, bits));
            Reads::cells(func, inline, &[], reads.term(b), |c| cell_bit(c, bits));
        },
    );
    let mut conflict = vec![false; k_count];
    liveness.walk(|id, live| {
        func.insts[id].cell_writes_each(&mut |c| {
            if ci[raw(c)] == u32::MAX {
                return;
            }
            let copies = match func.insts[id] {
                Inst::Store { value, .. } => Some(value),
                _ => None,
            };
            for &k in &by_cell[ci[raw(c)] as usize - k_count] {
                let k = k as usize;
                if live.get(k) && copies != Some(vals[k]) {
                    conflict[k] = true;
                }
            }
        });
        if vi[raw(id)] != u32::MAX {
            let k = vi[raw(id)] as usize;
            let bit = ci[raw(cells[k])] as usize;
            // The cell must be dead here, and no other value of the cell may
            // still be live.
            if live.get(bit)
                || by_cell[bit - k_count]
                    .iter()
                    .any(|&o| o as usize != k && live.get(o as usize))
            {
                conflict[k] = true;
            }
        }
    });
    if conflict.iter().all(|&c| c) {
        return Vec::new();
    }
    let mut alias = vec![None; func.insts.len()];
    for k in 0..k_count {
        if !conflict[k] {
            alias[raw(vals[k])] = Some(cells[k]);
        }
    }
    alias
}

/// A call whose result nothing reads runs as a bare call statement, with no
/// register to assign and then kill. Its callee must render as a name: a
/// statement opening with `(` would continue the previous line's expression.
/// A folded cell read prints as the cell's name.
fn result_discarded(func: &MirFunc, inline: &[bool], used: &[bool], id: Idx<Inst>) -> bool {
    !used[raw(id)]
        && match func.insts[id] {
            Inst::Call { .. } => true,
            Inst::CallValue { callee, .. } => {
                !inline[raw(callee)] || matches!(func.insts[callee], Inst::Load(_))
            }
            _ => false,
        }
}

/// Result of [`color_values`]: the per-inst color table plus, per color, the
/// counted loop that lexically owns it (`None` = declared at function top).
pub(crate) struct ValueAlloc {
    /// Dense `raw(inst) -> color` (`u32::MAX` = uncolored: inlined or non-value).
    pub colors: Vec<u32>,
    /// `color -> owning loop header`. A `Some(h)` color's `local r<c>` is
    /// declared inside loop `h`'s `for ... do` block, not at function top:
    /// LuaJIT's snapshot liveness (`snap_usedef`) prunes slots above a jump's
    /// free-slot watermark, so body-scoped temps disappear from loop-exit
    /// snapshots instead of pinning their values into every side trace — past
    /// ~16 inherited FP values `asm_head_side` aborts the side trace with
    /// "NYI: register coalescing too complex" and the trace tree blacklists.
    pub color_scope: Vec<Option<Idx<Block>>>,
}

/// Cells an instruction DEFINES (writes).
pub(crate) fn cell_defs(inst: &Inst, push: &mut impl FnMut(Idx<Cell>)) {
    match inst {
        Inst::Store { cell, .. } => push(*cell),
        Inst::CallMulti { dests, .. } => dests.iter().for_each(|d| push(*d)),
        Inst::CallValueMulti { dests, .. } => dests.iter().for_each(|d| push(*d)),
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            push(*key_cell);
            push(*val_cell);
        }
        _ => {}
    }
}

/// Cells an instruction USES (reads). `MapNext` reads its key cell (the cursor)
/// before writing it.
pub(crate) fn cell_uses(inst: &Inst, push: &mut impl FnMut(Idx<Cell>)) {
    match inst {
        Inst::Load(c) => push(*c),
        Inst::MapNext { key_cell, .. } => push(*key_cell),
        _ => {}
    }
}

/// The cell a value is computed in instead of a register of its own (see
/// [`store_targets`]); an empty table means none is.
pub(crate) fn alias_at(alias: &[Option<Idx<Cell>>], v: Idx<Inst>) -> Option<Idx<Cell>> {
    alias.get(raw(v)).copied().flatten()
}

fn inlined_operand_cell_uses(
    func: &MirFunc,
    inline: &[bool],
    alias: &[Option<Idx<Cell>>],
    inst: &Inst,
    push: &mut impl FnMut(Idx<Cell>),
) {
    inst_uses(inst, &mut |value| {
        if let Some(cell) = alias_at(alias, value) {
            push(cell);
            return;
        }
        if !inline[raw(value)] {
            return;
        }
        match &func.insts[value] {
            Inst::Load(cell) => push(*cell),
            nested => inlined_operand_cell_uses(func, inline, alias, nested, push),
        }
    });
}

/// The cells statement `id` writes, counting a value computed in a cell.
fn emitted_cell_defs(
    func: &MirFunc,
    alias: &[Option<Idx<Cell>>],
    id: Idx<Inst>,
    push: &mut impl FnMut(Idx<Cell>),
) {
    cell_defs(&func.insts[id], push);
    if let Some(cell) = alias_at(alias, id) {
        push(cell);
    }
}

/// The registers and cells of one body in one dense bit space -- registers
/// first, cells from the next word boundary -- with what each statement and
/// terminator defines and reads there, the bits live into each block, and
/// the interference within each kind. Registers and cells are colored apart,
/// but their liveness is one problem over one walk of the body.
///
/// A register is a value-producing instruction in `scope` (the reachable
/// blocks of one function or closure body) that the emitter gives a local:
/// one `inline` does not tree-fold into its use, `alias` does not compute in a
/// cell, and whose result something reads. A use of a folded value is seen
/// THROUGH to the registers and cells its expression reads (via [`Reads`]),
/// so those stay live up to the using statement.
///
/// Cells are mutable variables (Lua locals): `Store`/`CallMulti` dest/
/// `MapNext` write = a def, `Load`/`MapNext` cursor = a use, a param is
/// defined at entry. This is the classic live-variable interference (multiple
/// defs, unlike SSA values), so two cells whose live ranges are disjoint share
/// one `c<color>`. Without this each cell is its own local and a big function
/// blows luajit's 200-local parse cap (a large update function can need 193
/// cells).
pub(crate) struct Locals<'a> {
    func: &'a MirFunc,
    scope: &'a [Idx<Block>],
    flow: &'a Flow,
    inline: &'a [bool],
    /// Registers in emit-id order, the order they take colors in.
    values: Vec<Idx<Inst>>,
    /// Logical cell index -> cell. A compact inline template leaves holes in
    /// the logical source arena; they stay as zero-edge virtual cells so
    /// removing them does not renumber later colors.
    cells: Vec<Option<Idx<Cell>>>,
    /// Dense physical cell -> logical index.
    ci: Vec<u32>,
    /// Words of a bit row that hold registers; cells follow.
    v_words: usize,
    /// The bits each statement defines and then reads, block by block, each
    /// block closed by an entry for what its terminator reads.
    bits: Vec<u32>,
    /// Per entry: where its defs start and where its uses start.
    marks: Vec<(u32, u32)>,
    /// Per block: its first entry; its last is the terminator's.
    first: Vec<u32>,
    /// Per block: upward-exposed uses, defs, and the bits live on entry.
    use_set: BitRows,
    def_set: BitRows,
    live_in: BitRows,
    value_graph: Neighbors,
    cell_graph: Neighbors,
    /// By logical cell index: the cell owns a color alone.
    force: Vec<bool>,
}

impl<'a> Locals<'a> {
    /// `pinned` and `extra_pinned` cells never share a color; the second
    /// names cells the emitter references implicitly (a coroutine's frame
    /// parameter).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        func: &'a MirFunc,
        scope: &'a [Idx<Block>],
        flow: &'a Flow,
        inline: &'a [bool],
        alias: &[Option<Idx<Cell>>],
        pinned: &[bool],
        extra_pinned: &[Idx<Cell>],
        reads: &Reads,
    ) -> Self {
        let n = scope.len();
        // Only placed instructions count as readers: a transform can leave a
        // dropped instruction allocated in the arena, still naming its operands.
        let mut used = vec![false; func.insts.len()];
        for (_, block) in func.blocks.iter() {
            for &id in &block.insts {
                func.insts[id].uses_each(&mut |v| used[raw(v)] = true);
            }
            block.term.uses_each(&mut |v| used[raw(v)] = true);
        }
        let mut placed = 0;
        let mut values: Vec<Idx<Inst>> = Vec::new();
        for &b in scope {
            placed += func.blocks[b].insts.len();
            for &id in &func.blocks[b].insts {
                let inst = &func.insts[id];
                if inst.produces_value()
                    && !inline[raw(id)]
                    && alias_at(alias, id).is_none()
                    && !result_discarded(func, inline, &used, id)
                {
                    values.push(id);
                }
            }
        }
        values.sort_unstable_by_key(|v| func.emit_inst_id(*v));
        values.dedup();
        let v_count = values.len();
        let mut vi = vec![u32::MAX; func.insts.len()];
        for (i, &v) in values.iter().enumerate() {
            vi[raw(v)] = i as u32;
        }
        let v_words = v_count.div_ceil(64);
        let cell_base = (v_words * 64) as u32;

        // EVERY cell of the function takes a color (an ordinary fn has no
        // frame cells), in logical order.
        let c_count = func.emit_cell_span() as usize;
        let mut cells: Vec<Option<Idx<Cell>>> = vec![None; c_count];
        let mut ci = vec![u32::MAX; func.cells.len()];
        for (id, _) in func.cells.iter() {
            let i = func.emit_cell_id(id);
            cells[i as usize] = Some(id);
            ci[raw(id)] = i;
        }
        let bit_count = cell_base as usize + c_count;

        // The bits each statement defines and then reads, then the bits its
        // block's terminator reads, flattened once for the passes below.
        // Seeing through folded operands is the expensive part of any of
        // them. A folded statement keeps only its cell writes: the suppressed
        // store of a forwarded tail call still clobbers its cell's color.
        let mut bits: Vec<u32> = Vec::with_capacity(placed + reads.leaves.len());
        let mut marks: Vec<(u32, u32)> = Vec::with_capacity(placed + n + 1);
        let mut first: Vec<u32> = Vec::with_capacity(n + 1);
        let push_reads = |bits: &mut Vec<u32>, leaves: &[u32]| {
            Reads::values(func, inline, leaves, |v| {
                // A value computed in a cell has no register.
                if vi[raw(v)] != u32::MAX {
                    bits.push(vi[raw(v)]);
                }
            });
            Reads::cells(func, inline, alias, leaves, |c| {
                bits.push(cell_base + ci[raw(c)])
            });
        };
        for &b in scope {
            first.push(marks.len() as u32);
            for &id in &func.blocks[b].insts {
                let start = bits.len() as u32;
                let folded = inline[raw(id)];
                if vi[raw(id)] != u32::MAX {
                    bits.push(vi[raw(id)]);
                }
                emitted_cell_defs(func, alias, id, &mut |c| bits.push(cell_base + ci[raw(c)]));
                marks.push((start, bits.len() as u32));
                if !folded {
                    push_reads(&mut bits, reads.stmt(id));
                }
            }
            let start = bits.len() as u32;
            marks.push((start, start));
            push_reads(&mut bits, reads.term(b));
        }
        first.push(marks.len() as u32);
        marks.push((bits.len() as u32, bits.len() as u32));
        let defs = |e: usize| &bits[marks[e].0 as usize..marks[e].1 as usize];
        let uses = |e: usize| &bits[marks[e].1 as usize..marks[e + 1].0 as usize];

        // A statement reads before it writes (a `MapNext` reads the cursor it
        // then advances), so its uses are upward-exposed past its own defs.
        let mut use_set = BitRows::new(n, bit_count);
        let mut def_set = BitRows::new(n, bit_count);
        for bi in 0..n {
            for e in first[bi] as usize..first[bi + 1] as usize {
                for &k in uses(e) {
                    if !def_set.get(bi, k as usize) {
                        use_set.set(bi, k as usize);
                    }
                }
                for &k in defs(e) {
                    def_set.set(bi, k as usize);
                }
            }
        }
        let live_in = flow.live_in(&use_set, &def_set);

        let mut force = vec![false; c_count];
        for (i, c) in cells.iter().enumerate() {
            if let Some(c) = c {
                force[i] = pinned[raw(*c)];
            }
        }
        for &cell in extra_pinned {
            force[ci[raw(cell)] as usize] = true;
        }
        // Forced emit ids may name a hole in a compact template's arena; such
        // a hole still consumes a unique color so later colors keep their
        // numbers.
        if let Some(ids) = func.emit_ids.as_ref() {
            for &emit_id in &ids.forced_cells {
                if let Some(slot) = force.get_mut(emit_id as usize) {
                    *slot = true;
                }
            }
        }
        // Interference edges between NON-force-unique cells only: a
        // force-unique cell gets a dedicated color in `0..base` and every
        // other cell's search starts at `base`, so edges touching a force
        // cell never change a coloring result.
        let mut allowed = BitSet::new(c_count);
        for (i, &force) in force.iter().enumerate() {
            if !force {
                allowed.set(i);
            }
        }

        // Interference, collected from a single backward walk: a register or
        // cell interferes with everything of its kind live where it is
        // defined.
        let mut value_graph = Interference::new(v_count);
        let mut cell_graph = Interference::new(c_count);
        let mut live = BitSet::new(bit_count);
        for (bi, &b) in scope.iter().enumerate() {
            flow.live_out(&live_in, bi, &mut live);
            let term = first[bi + 1] as usize - 1;
            for &k in uses(term) {
                live.set(k as usize);
            }
            for e in (first[bi] as usize..term).rev() {
                for &d in defs(e) {
                    if d < cell_base {
                        live.remove(d as usize);
                        value_graph.add_live(d as usize, &live.words[..v_words], None);
                    } else if !force[(d - cell_base) as usize] {
                        cell_graph.add_live(
                            (d - cell_base) as usize,
                            &live.words[v_words..],
                            Some(&allowed.words),
                        );
                    }
                }
                for &d in defs(e) {
                    live.remove(d as usize);
                }
                for &k in uses(e) {
                    live.set(k as usize);
                }
            }
            // Params are live from entry: interfere them with live-in to
            // entry and with one another.
            if b == func.entry() {
                for &p in &func.params {
                    let pi = ci[raw(p)] as usize;
                    if force[pi] {
                        continue;
                    }
                    cell_graph.add_live(pi, &live.words[v_words..], Some(&allowed.words));
                    for &q in &func.params {
                        let qi = ci[raw(q)] as usize;
                        if pi != qi && !force[qi] {
                            cell_graph.add(pi, qi);
                        }
                    }
                }
            }
        }
        Locals {
            func,
            scope,
            flow,
            inline,
            values,
            cells,
            ci,
            v_words,
            bits,
            marks,
            first,
            use_set,
            def_set,
            live_in,
            value_graph: value_graph.finish(v_count),
            cell_graph: cell_graph.finish(c_count),
            force,
        }
    }

    /// The bits entry `e` defines.
    fn defs(&self, e: usize) -> &[u32] {
        &self.bits[self.marks[e].0 as usize..self.marks[e].1 as usize]
    }

    /// The bits entry `e` reads.
    fn uses(&self, e: usize) -> &[u32] {
        &self.bits[self.marks[e].1 as usize..self.marks[e + 1].0 as usize]
    }

    /// Color every register: values that never interfere share a color, and
    /// the emitter renders a colored value as `r<color>`. Coloring is per
    /// scope, so a closure's values are independent of the enclosing
    /// function's (only cells cross the boundary).
    ///
    /// `loops` partitions the coloring: a value whose defs and effective uses
    /// all sit inside one counted loop's body (and which is not live across
    /// the back edge) is colored from that loop's own palette, and the color
    /// is reported in [`ValueAlloc::color_scope`] so the emitter declares it
    /// inside the `for` block. Palettes are disjoint, so a loop-scoped local
    /// never shares a name with (or shadows) a function-top local.
    pub(crate) fn color_values(&self, loops: &[LoopScope]) -> ValueAlloc {
        let (func, blocks) = (self.func, self.scope);
        let n = blocks.len();
        let v_count = self.values.len();
        let value_bits = |rows: &BitRows, bi: usize, f: &mut dyn FnMut(usize)| {
            for_each_bit(&rows.row(bi)[..self.v_words], f)
        };

        // ---- Partition values by innermost enclosing counted loop. ----
        // Partition id: 0 = function top; k+1 = loops[k]. A block's partition is the
        // innermost loop whose region holds it; a value's partition is the loop-nest
        // LCA of every block it's defined or effectively used in. Values live into a
        // loop header (back-edge liveness: cross-iteration values a body-scoped Lua
        // `local` would re-nil each iteration) are hoisted above that loop.
        let mut block_part = vec![0u32; func.blocks.len()];
        for (k, ls) in loops.iter().enumerate() {
            for &b in &ls.region {
                let cur = block_part[raw(b)];
                let tighter = cur == 0 || {
                    let c = &loops[cur as usize - 1];
                    (c.region.len(), func.emit_block_id(c.header))
                        > (ls.region.len(), func.emit_block_id(ls.header))
                };
                if tighter {
                    block_part[raw(b)] = k as u32 + 1;
                }
            }
        }
        let parent_of = |p: u32| -> u32 {
            if p == 0 {
                0
            } else {
                block_part[raw(loops[p as usize - 1].header)]
            }
        };
        let depth_of = |mut p: u32| -> u32 {
            let mut d = 0;
            while p != 0 {
                p = parent_of(p);
                d += 1;
            }
            d
        };
        let lca = |mut a: u32, mut b: u32| -> u32 {
            let (mut da, mut db) = (depth_of(a), depth_of(b));
            while da > db {
                a = parent_of(a);
                da -= 1;
            }
            while db > da {
                b = parent_of(b);
                db -= 1;
            }
            while a != b {
                a = parent_of(a);
                b = parent_of(b);
            }
            a
        };
        // u32::MAX = value not seen yet (every colored value has a def, so all end assigned).
        let mut val_part: Vec<u32> = vec![u32::MAX; v_count];
        if !loops.is_empty() {
            for bi in 0..n {
                let p = block_part[raw(blocks[bi])];
                let mut fold = |k: usize| {
                    let cur = val_part[k];
                    val_part[k] = if cur == u32::MAX { p } else { lca(cur, p) };
                };
                value_bits(&self.def_set, bi, &mut fold);
                value_bits(&self.use_set, bi, &mut fold);
            }
            for (k, ls) in loops.iter().enumerate() {
                let Some(hbi) = self.flow.index(ls.header) else {
                    continue;
                };
                let lp = k as u32 + 1;
                value_bits(&self.live_in, hbi, &mut |v| {
                    let mut p = val_part[v];
                    if p == u32::MAX {
                        return;
                    }
                    // If v's partition is this loop or nested inside it, hoist to
                    // this loop's parent.
                    let mut inside = false;
                    while p != 0 {
                        if p == lp {
                            inside = true;
                            break;
                        }
                        p = parent_of(p);
                    }
                    if inside {
                        val_part[v] = parent_of(lp);
                    }
                });
            }
        }

        // Greedy coloring in arena order for deterministic output. `used` is a dense
        // bool scratch (colors are small ints), reused across values so each one pays
        // no per-value HashSet alloc/hash. The result is a dense `raw(inst) -> color`
        // table (`u32::MAX` = no register: inlined or non-value), so the emitter reads
        // a value's color by array index instead of hashing per operand reference.
        // A value only takes a color its partition owns (`color_part`), so top-level
        // and loop-scoped values never share a Lua local.
        let mut colors: Vec<u32> = vec![u32::MAX; func.insts.len()];
        let mut color_of: Vec<u32> = vec![u32::MAX; v_count];
        let mut color_part: Vec<u32> = Vec::new();
        let mut used: Vec<bool> = Vec::new();
        for (i, &v) in self.values.iter().enumerate() {
            used.clear();
            self.value_graph.for_each(i, |nb| {
                let c = color_of[nb];
                if c != u32::MAX {
                    let c = c as usize;
                    if c >= used.len() {
                        used.resize(c + 1, false);
                    }
                    used[c] = true;
                }
            });
            let my = match val_part[i] {
                u32::MAX => 0,
                p => p,
            };
            let mut c = 0u32;
            loop {
                let cu = c as usize;
                if cu >= color_part.len() {
                    color_part.push(my); // mint a new color owned by this partition
                    break;
                }
                if color_part[cu] == my && !(cu < used.len() && used[cu]) {
                    break;
                }
                c += 1;
            }
            colors[raw(v)] = c;
            color_of[i] = c;
        }
        let color_scope = color_part
            .iter()
            .map(|&p| {
                if p == 0 {
                    None
                } else {
                    Some(loops[p as usize - 1].header)
                }
            })
            .collect();
        ValueAlloc {
            colors,
            color_scope,
        }
    }

    /// Color the cells: a dense `raw(cell) -> color` table (`u32::MAX` =
    /// uncolored). Non-interfering cells share a color; the emitter renders a
    /// colored cell as `c<color>`. Cells in detached nested CFGs are forced
    /// to a unique color and are never emitted with their enclosing body.
    pub(crate) fn color_cells(&self) -> Vec<u32> {
        let (func, ci, force) = (self.func, &self.ci, &self.force);
        // A force-unique cell conflicts with EVERY cell (a closure captures the local
        // by identity, so it must never be reused). Rather than materialize O(F x C)
        // interference edges, give each force-unique cell its own color 0..F-1 and
        // start every other cell's search at F — equivalent, but avoids the edge
        // explosion that once dominated this pass on inlined functions.
        // Dense `raw(cell) -> color` (`u32::MAX` = uncolored), so the emitter reads a
        // cell's color by array index instead of hashing per `Load`/`Store`.
        let mut colors: Vec<u32> = vec![u32::MAX; func.cells.len()];
        let mut color_of: Vec<u32> = vec![u32::MAX; self.cells.len()];
        let mut next_unique = 0u32;
        for (i, c) in self.cells.iter().enumerate() {
            if force[i] {
                if let Some(c) = c {
                    colors[raw(*c)] = next_unique;
                }
                color_of[i] = next_unique;
                next_unique += 1;
            }
        }
        let base = next_unique;

        // Greedy-color the rest from `base` up, over their real (non-force-unique)
        // interference. Edges to force-unique cells are irrelevant — those colors are
        // all below `base`. Arena order for deterministic output. `used` is a dense
        // bool scratch reused across cells (no per-cell HashSet alloc).
        // A copy between two cells that share a color prints nothing, so a cell
        // takes the color of a copy partner when no neighbor holds it.
        let mut partners: Vec<(u32, u32)> = Vec::new();
        for &b in self.scope {
            for &id in &func.blocks[b].insts {
                if let Inst::Store { cell, value } = func.insts[id]
                    && self.inline[raw(value)]
                    && let Inst::Load(source) = func.insts[value]
                {
                    let (a, b) = (ci[raw(cell)], ci[raw(source)]);
                    partners.push((a, b));
                    partners.push((b, a));
                }
            }
        }
        partners.sort_unstable();
        partners.dedup();
        let mut used: Vec<bool> = Vec::new();
        for (i, c) in self.cells.iter().enumerate() {
            if force[i] {
                continue;
            }
            used.clear();
            self.cell_graph.for_each(i, |nb| {
                let col = color_of[nb];
                if col != u32::MAX {
                    let col = col as usize;
                    if col >= used.len() {
                        used.resize(col + 1, false);
                    }
                    used[col] = true;
                }
            });
            let free = |col: usize| col >= base as usize && used.get(col) != Some(&true);
            let start = partners.partition_point(|&(a, _)| (a as usize) < i);
            let preferred = partners[start..]
                .iter()
                .take_while(|&&(a, _)| a as usize == i)
                .map(|&(_, b)| color_of[b as usize])
                .find(|&col| col != u32::MAX && free(col as usize));
            let mut col = base as usize;
            while col < used.len() && used[col] {
                col += 1;
            }
            if let Some(p) = preferred {
                col = p as usize;
            }
            if let Some(c) = c {
                colors[raw(*c)] = col as u32;
            }
            color_of[i] = col as u32;
        }
        colors
    }
}

/// Color the cells of a function that has no emitter-only cell uses.
#[cfg(test)]
pub(crate) fn color_cells(func: &MirFunc) -> Vec<u32> {
    let order = crate::reverse_postorder(func, func.entry());
    let flow = Flow::new(func, &order);
    let pinned = pinned_cells(func, &flow);
    let inline = vec![false; func.insts.len()];
    let reads = Reads::new(func, &order, &inline);
    Locals::new(func, &order, &flow, &inline, &[], &pinned, &[], &reads).color_cells()
}

/// Cells held by reference for the whole body -- a closure's params, any cell
/// a detached (closure) sub-CFG touches, and cells the emit-id table forces --
/// as a dense `raw(cell) -> bool`. They never share a color and are never
/// nil-killed: the closure reads the local by identity, so a kill would be
/// visible through it.
pub(crate) fn pinned_cells(func: &MirFunc, flow: &Flow) -> Vec<bool> {
    let mut pinned = vec![false; func.cells.len()];
    for c in collect_lambda_params(func) {
        pinned[raw(c)] = true;
    }
    for (bid, b) in func.blocks.iter() {
        if flow.index(bid).is_some() {
            continue;
        }
        for &id in &b.insts {
            cell_defs(&func.insts[id], &mut |c| pinned[raw(c)] = true);
            cell_uses(&func.insts[id], &mut |c| pinned[raw(c)] = true);
        }
    }
    if let Some(ids) = func.emit_ids.as_ref() {
        let forced: HashSet<u32> = ids.forced_cells.iter().copied().collect();
        for (id, _) in func.cells.iter() {
            if forced.contains(&func.emit_cell_id(id)) {
                pinned[raw(id)] = true;
            }
        }
    }
    pinned
}

// --- nil-kill: where each emitted local dies ------------------------------
//
// LuaJIT assembles a side trace only if every non-constant slot in the parent
// trace's exit snapshot fits a register at the side-trace head (13 GPR / 16 FPR
// on x64, `asm_head_side`), and snapshot pruning covers only the innermost
// frame up to its first jump. A local assigned `nil` after its last use is a
// snapshot constant from then on, in every frame of the inlined chain
// (`benchmarks/luajit/trace-aborts/calleekill.lua`). This pass computes, over
// the locals the emitter actually declares (value colors `r<k>`, cell colors
// `c<k>`), the point at which each one dies; the emitter writes `<local> = nil`
// there. Kills are placed only where the local is dead on every path, so a
// missed kill costs pressure and a misplaced one would be a miscompile.

/// One emitted Lua local: a value register `r<color>` or a cell local `c<color>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KillLocal {
    Value(u32),
    Cell(u32),
}

pub(crate) struct NilKills<'a> {
    /// Dense `raw(inst) -> range of after_locals`: the locals dead right after
    /// that instruction's statement. One list for the body instead of a vector
    /// per instruction.
    after_range: Vec<(u32, u32)>,
    after_locals: Vec<KillLocal>,
    /// Dense `raw(block) -> range of entry_locals`: the locals dead on entry
    /// to the block that some predecessor still had live (dying on the edge
    /// or at its terminator).
    entry_range: Vec<(u32, u32)>,
    entry_locals: Vec<KillLocal>,
    /// Per scope block: live-in slots, and slots live at the block's end
    /// (successor live-in plus the terminator's own reads).
    live_in: BitRows,
    end_live: BitRows,
    flow: Option<&'a Flow>,
    /// Per scope block: the range of `first_pure_slots` holding the slots
    /// redefined by its leading call-free statements, so an entry kill of them
    /// would be wasted.
    first_pure_range: Vec<(u32, u32)>,
    first_pure_slots: Vec<usize>,
    value_count: u32,
    value_scope: Vec<Option<Idx<Block>>>,
    loop_blocks: Vec<(Idx<Block>, HashSet<Idx<Block>>)>,
    /// Dense `raw(block)`: control reaches a `return` from the block's entry
    /// without running a statement.
    return_tail: Vec<bool>,
}

impl NilKills<'_> {
    /// No kills anywhere (coroutine `__step` bodies, or the pass disabled).
    pub(crate) fn none() -> Self {
        NilKills {
            after_range: Vec::new(),
            after_locals: Vec::new(),
            entry_range: Vec::new(),
            entry_locals: Vec::new(),
            live_in: BitRows::new(0, 0),
            end_live: BitRows::new(0, 0),
            flow: None,
            first_pure_range: Vec::new(),
            first_pure_slots: Vec::new(),
            value_count: 0,
            value_scope: Vec::new(),
            loop_blocks: Vec::new(),
            return_tail: Vec::new(),
        }
    }

    /// Mark the blocks that reach a `return` without running a statement
    /// (`quiet` says which instructions print none) and drop their entry
    /// kills: a return drops every slot itself, and nothing between can exit.
    pub(crate) fn mark_return_tails(&mut self, func: &MirFunc, quiet: impl Fn(Idx<Inst>) -> bool) {
        let mut tail = vec![false; func.blocks.len()];
        let mut changed = true;
        while changed {
            changed = false;
            for (b, block) in func.blocks.iter() {
                if tail[raw(b)] || !block.insts.iter().all(|&id| quiet(id)) {
                    continue;
                }
                let reaches = match block.term {
                    Term::Return(_) | Term::Unreachable => true,
                    Term::Goto(s) => tail[raw(s)],
                    _ => false,
                };
                if reaches {
                    tail[raw(b)] = true;
                    changed = true;
                }
            }
        }
        for (range, &tail) in self.entry_range.iter_mut().zip(&tail) {
            if tail {
                *range = (0, 0);
            }
        }
        self.return_tail = tail;
    }

    pub(crate) fn reaches_return(&self, b: Idx<Block>) -> bool {
        self.return_tail.get(raw(b)).copied().unwrap_or(false)
    }

    /// No kills in `b`, on entry or after any of its statements.
    pub(crate) fn drop_block(&mut self, func: &MirFunc, b: Idx<Block>) {
        if let Some(range) = self.entry_range.get_mut(raw(b)) {
            *range = (0, 0);
        }
        for &id in &func.blocks[b].insts {
            if let Some(range) = self.after_range.get_mut(raw(id)) {
                *range = (0, 0);
            }
        }
    }

    /// Locals dead on entry to `b` that a predecessor still had live.
    pub(crate) fn at_entry(&self, b: Idx<Block>) -> &[KillLocal] {
        match self.entry_range.get(raw(b)) {
            Some(&(start, end)) => &self.entry_locals[start as usize..end as usize],
            None => &[],
        }
    }

    /// Locals dead right after the statement of `id`.
    pub(crate) fn after_inst(&self, id: Idx<Inst>) -> &[KillLocal] {
        match self.after_range.get(raw(id)) {
            Some(&(start, end)) => &self.after_locals[start as usize..end as usize],
            None => &[],
        }
    }

    fn decode(&self, slot: usize) -> KillLocal {
        if (slot as u32) < self.value_count {
            KillLocal::Value(slot as u32)
        } else {
            KillLocal::Cell(slot as u32 - self.value_count)
        }
    }

    /// A loop-scoped register is declared inside its `for` block, so a kill
    /// anywhere else would assign a global of the same name.
    fn allowed_at(&self, local: KillLocal, block: Idx<Block>) -> bool {
        match local {
            KillLocal::Value(c) => match self.value_scope.get(c as usize).copied().flatten() {
                None => true,
                Some(h) => self
                    .loop_blocks
                    .iter()
                    .any(|(header, blocks)| *header == h && blocks.contains(&block)),
            },
            KillLocal::Cell(_) => true,
        }
    }

    /// Locals live at the end of `from` that are dead on entry to `to`, for a
    /// control-flow edge the emitter renders as a direct fall-through (the
    /// `math.min`/`math.max` selection skips its arm blocks).
    pub(crate) fn on_edge(&self, from: Idx<Block>, to: Idx<Block>) -> Vec<KillLocal> {
        let mut out = Vec::new();
        self.each_on_edge(from, to, |local| out.push(local));
        out
    }

    fn each_on_edge(&self, from: Idx<Block>, to: Idx<Block>, f: impl FnMut(KillLocal)) {
        let Some(flow) = self.flow else {
            return;
        };
        if let (Some(fi), Some(ti)) = (flow.index(from), flow.index(to)) {
            self.each_on_edge_at(fi, ti, to, f);
        }
    }

    /// [`Self::each_on_edge`] by the blocks' indices in the flow.
    fn each_on_edge_at(&self, fi: usize, ti: usize, to: Idx<Block>, mut f: impl FnMut(KillLocal)) {
        let (start, end) = self.first_pure_range[ti];
        let first_pure = &self.first_pure_slots[start as usize..end as usize];
        for (wi, (&e, &l)) in self
            .end_live
            .row(fi)
            .iter()
            .zip(self.live_in.row(ti))
            .enumerate()
        {
            let mut bits = e & !l;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                let slot = wi * 64 + b;
                let local = self.decode(slot);
                if self.allowed_at(local, to) && !first_pure.contains(&slot) {
                    f(local);
                }
                bits &= bits - 1;
            }
        }
    }
}

/// Whether the statement for `id` (with the operands folded into it) runs no
/// Lua call: a trace exit inside it can only be at the statement itself.
/// Conservative allowlist; anything unlisted counts as a possible call.
pub(crate) fn statement_is_pure(func: &MirFunc, inline: &[bool], id: Idx<Inst>) -> bool {
    let inst = &func.insts[id];
    let pure_kind = matches!(
        inst,
        Inst::Const(_)
            | Inst::Global(_)
            | Inst::Load(_)
            | Inst::Store { .. }
            | Inst::Field { .. }
            | Inst::TupleIndex { .. }
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
            | Inst::UintBitNot(_)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
    );
    if !pure_kind {
        return false;
    }
    let mut pure = true;
    inst_uses(inst, &mut |u| {
        if inline[raw(u)] && !statement_is_pure(func, inline, u) {
            pure = false;
        }
    });
    pure
}

/// Compute the death point of every emitted local of one function body.
/// `quiet` marks the instructions that emit no statement of their own when
/// that is more than the values `locals` already folds (hoisted constants, a
/// counted loop's header and latch); their reads are attributed to the
/// consumer, as the emitter renders them. `loops` supplies the blocks in
/// which a loop-scoped register may be killed (its `for` body plus the latch
/// label, both inside the `for` block). `cell_excluded` is every cell this
/// body does NOT render as a bare `c<color>` local, from the caller's own
/// declaration predicate: a kill of one of those would assign a GLOBAL of
/// that name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nil_kills<'a>(
    locals: &Locals<'a>,
    quiet: Option<&[bool]>,
    value_colors: &[u32],
    value_scope: &[Option<Idx<Block>>],
    loops: &[LoopScope],
    cell_colors: &[u32],
    cell_excluded: &[bool],
    params: &[Idx<Cell>],
    alias: &[Option<Idx<Cell>>],
    reads: &Reads,
) -> NilKills<'a> {
    let (func, scope, flow) = (locals.func, locals.scope, locals.flow);
    let inline = quiet.unwrap_or(locals.inline);
    let n = scope.len();
    // Every color is held by some register, so the palette is the count.
    let value_count = value_scope.len() as u32;
    // Slot space: value colors first, then killable cell colors. `cell_excluded`
    // carries both reasons a cell is not killable: it is pinned (owning its color
    // alone, so excluding it by cell excludes the color), or the emitter renders
    // it as no local at all, in which case a kill would assign a global.
    let mut cell_slot = vec![u32::MAX; func.cells.len()];
    let mut cell_count = 0u32;
    for (id, _) in func.cells.iter() {
        let col = cell_colors[raw(id)];
        if col != u32::MAX && !cell_excluded[raw(id)] {
            cell_slot[raw(id)] = value_count + col;
            cell_count = cell_count.max(col + 1);
        }
    }
    let s_count = (value_count + cell_count) as usize;

    // The slots each emitted statement defines and then reads, per entry of
    // `locals`: the liveness pass and the backward kill walk both read them.
    let mut slots: Vec<u32> = Vec::with_capacity(locals.bits.len());
    let mut marks: Vec<(u32, u32)> = Vec::with_capacity(locals.marks.len());
    if quiet.is_none() {
        // The statements are the ones `locals` flattened, so each of its bits
        // names the slot directly.
        let cell_base = locals.v_words * 64;
        let mut slot_of = vec![u32::MAX; cell_base + locals.cells.len()];
        for (i, &v) in locals.values.iter().enumerate() {
            slot_of[i] = value_colors[raw(v)];
        }
        for (i, c) in locals.cells.iter().enumerate() {
            if let Some(c) = c {
                slot_of[cell_base + i] = cell_slot[raw(*c)];
            }
        }
        let named = |bits: &[u32], slots: &mut Vec<u32>| {
            slots.extend(
                bits.iter()
                    .map(|&k| slot_of[k as usize])
                    .filter(|&s| s != u32::MAX),
            )
        };
        let mut e = 0;
        for &b in scope {
            for &id in &func.blocks[b].insts {
                let start = slots.len() as u32;
                let mut mid = start;
                // A folded statement defines no slot: its entry holds only
                // the cell writes the colorer needs.
                if !inline[raw(id)] {
                    named(locals.defs(e), &mut slots);
                    mid = slots.len() as u32;
                    named(locals.uses(e), &mut slots);
                }
                marks.push((start, mid));
                e += 1;
            }
            let start = slots.len() as u32;
            marks.push((start, start));
            named(locals.uses(e), &mut slots);
            e += 1;
        }
    } else {
        let value_slot = |v: Idx<Inst>| -> Option<u32> {
            let c = value_colors[raw(v)];
            (c != u32::MAX).then_some(c)
        };
        let leaf_slots = |leaves: &[u32], slots: &mut Vec<u32>| {
            Reads::values(func, inline, leaves, |v| slots.extend(value_slot(v)));
            Reads::cells(func, inline, alias, leaves, |c| {
                if cell_slot[raw(c)] != u32::MAX {
                    slots.push(cell_slot[raw(c)]);
                }
            });
        };
        for &b in scope {
            for &id in &func.blocks[b].insts {
                let start = slots.len() as u32;
                if inline[raw(id)] {
                    marks.push((start, start));
                    continue;
                }
                if func.insts[id].produces_value() {
                    slots.extend(value_slot(id));
                }
                emitted_cell_defs(func, alias, id, &mut |c| {
                    if cell_slot[raw(c)] != u32::MAX {
                        slots.push(cell_slot[raw(c)]);
                    }
                });
                marks.push((start, slots.len() as u32));
                leaf_slots(reads.stmt(id), &mut slots);
            }
            let start = slots.len() as u32;
            marks.push((start, start));
            leaf_slots(reads.term(b), &mut slots);
        }
    }
    marks.push((slots.len() as u32, slots.len() as u32));
    let defs_at = |e: usize| &slots[marks[e].0 as usize..marks[e].1 as usize];
    let uses_at = |e: usize| &slots[marks[e].1 as usize..marks[e + 1].0 as usize];
    let first = &locals.first;

    let mut use_set = BitRows::new(n, s_count);
    let mut def_set = BitRows::new(n, s_count);
    for bi in 0..n {
        for e in first[bi] as usize..first[bi + 1] as usize {
            for &s in uses_at(e) {
                if !def_set.get(bi, s as usize) {
                    use_set.set(bi, s as usize);
                }
            }
            for &s in defs_at(e) {
                def_set.set(bi, s as usize);
            }
        }
    }
    let live_in = flow.live_in(&use_set, &def_set);

    let mut kills = NilKills {
        after_range: vec![(0, 0); func.insts.len()],
        after_locals: Vec::new(),
        entry_range: vec![(0, 0); func.blocks.len()],
        entry_locals: Vec::new(),
        live_in,
        end_live: BitRows::new(n, s_count),
        flow: Some(flow),
        first_pure_range: vec![(0, 0); n],
        first_pure_slots: Vec::new(),
        value_count,
        value_scope: value_scope.to_vec(),
        loop_blocks: loops
            .iter()
            .map(|l| (l.header, l.region.iter().copied().collect()))
            .collect(),
        return_tail: Vec::new(),
    };

    // Backward walk per block: a slot read or written by a statement and not
    // live after it dies there -- unless a later statement of the block
    // redefines it before any statement that can call. No exit can then
    // observe the stale value: an exit at one of those pure statements
    // re-executes from it, and LuaJIT's own snapshot pruning drops a slot the
    // straight-line continuation defines. Such a kill would only cost the
    // interpreter a `KPRI`.
    let mut live = BitSet::new(s_count);
    let mut dead: Vec<usize> = Vec::new();
    let mut next_pure_defs: Vec<usize> = Vec::new();
    for (bi, &b) in scope.iter().enumerate() {
        let block = &func.blocks[b];
        flow.live_out(&kills.live_in, bi, &mut live);
        let term = first[bi + 1] as usize - 1;
        for &s in uses_at(term) {
            live.set(s as usize);
        }
        kills.end_live.row_mut(bi).copy_from_slice(&live.words);
        next_pure_defs.clear();
        for (e, &id) in (first[bi] as usize..term).zip(&block.insts).rev() {
            if inline[raw(id)] {
                continue;
            }
            let (defs, uses) = (defs_at(e), uses_at(e));
            dead.clear();
            for &s in defs.iter().chain(uses) {
                let s = s as usize;
                if !live.get(s) && !dead.contains(&s) && !next_pure_defs.contains(&s) {
                    dead.push(s);
                }
            }
            if statement_is_pure(func, inline, id) {
                next_pure_defs.extend(defs.iter().map(|&s| s as usize));
            } else {
                next_pure_defs.clear();
            }
            let start = kills.after_locals.len() as u32;
            for &s in &dead {
                let local = kills.decode(s);
                kills.after_locals.push(local);
            }
            kills.after_range[raw(id)] = (start, kills.after_locals.len() as u32);
            for &s in defs {
                live.remove(s as usize);
            }
            for &s in uses {
                live.set(s as usize);
            }
        }
        // After the walk this holds the defs of the leading pure statements.
        let start = kills.first_pure_slots.len() as u32;
        kills.first_pure_slots.extend_from_slice(&next_pure_defs);
        kills.first_pure_range[bi] = (start, kills.first_pure_slots.len() as u32);
    }
    // Slots live at the end of a predecessor and dead on entry die on the edge;
    // the successor's label is the one place that covers every such edge.
    let mut entry: Vec<KillLocal> = Vec::new();
    for (ti, &to) in scope.iter().enumerate() {
        entry.clear();
        if to == func.entry() {
            // A parameter is defined at entry; one never read dies there.
            for &p in params {
                let s = cell_slot[raw(p)];
                if s != u32::MAX && !kills.live_in.get(ti, s as usize) {
                    let local = kills.decode(s as usize);
                    if !entry.contains(&local) {
                        entry.push(local);
                    }
                }
            }
        }
        for &fi in flow.preds(ti) {
            kills.each_on_edge_at(fi as usize, ti, to, |local| {
                if !entry.contains(&local) {
                    entry.push(local);
                }
            });
        }
        let start = kills.entry_locals.len() as u32;
        kills.entry_locals.extend_from_slice(&entry);
        kills.entry_range[raw(to)] = (start, kills.entry_locals.len() as u32);
    }
    kills
}

#[cfg(test)]
mod cell_color_tests {
    use super::*;
    use skull_mir::mir::Lit;
    use ustr::Ustr;

    /// Two cells whose live ranges do NOT overlap (`a` is read and dead before
    /// `b` is written) share one color.
    #[test]
    fn disjoint_cells_share_a_color() {
        let mut f = MirFunc::new(Ustr::from("t"));
        let e = f.entry();
        let a = f.new_cell(None);
        let b = f.new_cell(None);
        let one = f.emit(e, Inst::Const(Lit::Int(1)));
        f.emit(
            e,
            Inst::Store {
                cell: a,
                value: one,
            },
        );
        let la = f.emit(e, Inst::Load(a)); // a's last use -> dead after
        let na = f.emit(e, Inst::Neg(la));
        let two = f.emit(e, Inst::Const(Lit::Int(2)));
        f.emit(
            e,
            Inst::Store {
                cell: b,
                value: two,
            },
        ); // b born after a died
        let lb = f.emit(e, Inst::Load(b));
        let sum = f.emit(e, Inst::IntAdd(na, lb));
        f.set_term(e, Term::Return(vec![sum]));
        let colors = color_cells(&f);
        assert_eq!(
            colors[raw(a)],
            colors[raw(b)],
            "disjoint-lived cells should share a color"
        );
    }

    /// Two cells live at the same time (both read after both written) get
    /// distinct colors.
    #[test]
    fn overlapping_cells_get_distinct_colors() {
        let mut f = MirFunc::new(Ustr::from("t"));
        let e = f.entry();
        let a = f.new_cell(None);
        let b = f.new_cell(None);
        let one = f.emit(e, Inst::Const(Lit::Int(1)));
        f.emit(
            e,
            Inst::Store {
                cell: a,
                value: one,
            },
        );
        let two = f.emit(e, Inst::Const(Lit::Int(2)));
        f.emit(
            e,
            Inst::Store {
                cell: b,
                value: two,
            },
        );
        let la = f.emit(e, Inst::Load(a));
        let lb = f.emit(e, Inst::Load(b));
        let sum = f.emit(e, Inst::IntAdd(la, lb)); // a and b both live here
        f.set_term(e, Term::Return(vec![sum]));
        let colors = color_cells(&f);
        assert_ne!(
            colors[raw(a)],
            colors[raw(b)],
            "simultaneously-live cells must not share a color"
        );
    }
}
