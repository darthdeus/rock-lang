//! Liveness-based coloring of emitted value and cell locals.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::{Idx, RawIdx};

use crate::{ForControl, ForLoop, collect_lambda_params, successors};
use skull_mir::mir::{Block, Cell, Inst, MirFunc, Term};

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
    fn clear(&mut self) {
        self.words.iter_mut().for_each(|w| *w = 0);
    }
}

#[derive(Default)]
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

/// A statement reads before it writes (a `MapNext` reads the cursor it then
/// advances), so its uses are exposed past its own defs.
fn note_statement(use_row: &mut [u64], def_row: &mut [u64], uses: &[u32], defs: &[u32]) {
    for &k in uses {
        let (w, bit) = (k as usize >> 6, 1u64 << (k & 63));
        if def_row[w] & bit == 0 {
            use_row[w] |= bit;
        }
    }
    for &k in defs {
        def_row[k as usize >> 6] |= 1u64 << (k & 63);
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

pub(crate) struct Flow {
    /// `raw(block) -> index in the order`; `u32::MAX` for a block outside it.
    pos: Vec<u32>,
    succ_off: Vec<u32>,
    succ: Vec<u32>,
    /// One entry longer than `succ_off`: the fill cursor trails by a slot.
    pred_off: Vec<u32>,
    pred: Vec<u32>,
    placed: usize,
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
        let mut placed = 0;
        for &b in order {
            placed += func.blocks[b].insts.len();
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
            placed,
        }
    }

    fn len(&self) -> usize {
        self.succ_off.len() - 1
    }

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

    /// The least solution of `in = use | (out & !def)`, where a block's `out`
    /// is the union of its successors' `in`.
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

    fn live_out(&self, live_in: &BitRows, bi: usize, out: &mut BitSet) {
        out.clear();
        for &s in self.succs(bi) {
            for (word, &next) in out.words.iter_mut().zip(live_in.row(s as usize)) {
                *word |= next;
            }
        }
    }
}

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

/// A bit matrix grows with the square of the name count, so large bodies
/// collect an edge list instead.
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

#[inline]
fn raw<T>(i: Idx<T>) -> usize {
    u32::from(i.into_raw()) as usize
}

fn register_reads(func: &MirFunc, inline: &[bool], v: Idx<Inst>, push: &mut impl FnMut(Idx<Inst>)) {
    if inline[raw(v)] {
        func.insts[v].uses_each(&mut |u| register_reads(func, inline, u, push));
    } else {
        push(v);
    }
}

fn cell_reads(
    func: &MirFunc,
    inline: &[bool],
    alias: &[Option<Idx<Cell>>],
    v: Idx<Inst>,
    push: &mut impl FnMut(Idx<Cell>),
) {
    if let Some(cell) = alias_at(alias, v) {
        push(cell);
    } else if inline[raw(v)] {
        match &func.insts[v] {
            Inst::Load(cell) => push(*cell),
            nested => nested.uses_each(&mut |u| cell_reads(func, inline, alias, u, push)),
        }
    }
}

pub(crate) struct LoopScope {
    pub header: Idx<Block>,
    pub region: Vec<Idx<Block>>,
}

/// Items grouped by a key, as one chain per key threaded through the items.
struct Chains {
    /// `key -> its last item`, then `item -> the one before it`; `u32::MAX`
    /// ends a chain.
    head: Vec<u32>,
    prev: Vec<u32>,
}

impl Chains {
    fn new(keys: usize, key_of_item: impl Iterator<Item = usize>) -> Self {
        let mut head = vec![u32::MAX; keys];
        let prev = key_of_item
            .enumerate()
            .map(|(item, key)| std::mem::replace(&mut head[key], item as u32))
            .collect();
        Chains { head, prev }
    }

    /// The items of `key`, in no order a caller may rely on.
    fn of(&self, key: usize) -> impl Iterator<Item = usize> + '_ {
        let mut at = self.head[key];
        std::iter::from_fn(move || {
            (at != u32::MAX).then(|| {
                let item = at as usize;
                at = self.prev[item];
                item
            })
        })
    }
}

/// The bits each statement of a body defines and then reads, block by block,
/// each block closed by an entry for what its terminator reads, with the
/// backward liveness they imply.
struct Entries {
    bits: Vec<u32>,
    marks: Vec<(u32, u32)>,
    first: Vec<u32>,
    use_set: BitRows,
    def_set: BitRows,
    live_in: BitRows,
}

impl Entries {
    fn new(
        func: &MirFunc,
        scope: &[Idx<Block>],
        flow: &Flow,
        width: usize,
        mut defs: impl FnMut(usize, Idx<Inst>, &mut Vec<u32>),
        mut uses: impl FnMut(usize, Idx<Inst>, &mut Vec<u32>),
        mut term: impl FnMut(usize, Idx<Block>, &mut Vec<u32>),
    ) -> Self {
        let n = scope.len();
        let mut bits = Vec::new();
        let mut marks: Vec<(u32, u32)> = Vec::with_capacity(flow.placed + n + 1);
        let mut first = Vec::with_capacity(n + 1);
        let mut use_set = BitRows::new(n, width);
        let mut def_set = BitRows::new(n, width);
        for (bi, &b) in scope.iter().enumerate() {
            let (use_row, def_row) = (use_set.row_mut(bi), def_set.row_mut(bi));
            first.push(marks.len() as u32);
            for &id in &func.blocks[b].insts {
                let e = marks.len();
                let start = bits.len();
                defs(e, id, &mut bits);
                let mid = bits.len();
                uses(e, id, &mut bits);
                marks.push((start as u32, mid as u32));
                note_statement(use_row, def_row, &bits[mid..], &bits[start..mid]);
            }
            let start = bits.len();
            let e = marks.len();
            marks.push((start as u32, start as u32));
            term(e, b, &mut bits);
            note_statement(use_row, def_row, &bits[start..], &[]);
        }
        first.push(marks.len() as u32);
        marks.push((bits.len() as u32, bits.len() as u32));
        let live_in = flow.live_in(&use_set, &def_set);
        Entries {
            bits,
            marks,
            first,
            use_set,
            def_set,
            live_in,
        }
    }

    fn defs(&self, e: usize) -> &[u32] {
        &self.bits[self.marks[e].0 as usize..self.marks[e].1 as usize]
    }

    fn uses(&self, e: usize) -> &[u32] {
        &self.bits[self.marks[e].1 as usize..self.marks[e + 1].0 as usize]
    }

    fn live_set(&self) -> BitSet {
        BitSet {
            words: vec![0; self.live_in.width],
        }
    }

    /// Set `live` to the bits live just before block `bi`'s terminator.
    fn block_end(&self, flow: &Flow, bi: usize, live: &mut BitSet) {
        flow.live_out(&self.live_in, bi, live);
        for &k in self.uses(self.first[bi + 1] as usize - 1) {
            live.set(k as usize);
        }
    }

    /// Visit block `bi`'s statements backward from `live` at its end, each
    /// with its entry and the bits live just after it.
    fn walk_block(
        &self,
        bi: usize,
        insts: &[Idx<Inst>],
        live: &mut BitSet,
        mut at: impl FnMut(usize, Idx<Inst>, &BitSet),
    ) {
        let first = self.first[bi] as usize;
        for (e, &id) in (first..first + insts.len()).zip(insts).rev() {
            at(e, id, live);
            for &k in self.defs(e) {
                live.remove(k as usize);
            }
            for &k in self.uses(e) {
                live.set(k as usize);
            }
        }
    }

    fn walk(
        &self,
        func: &MirFunc,
        scope: &[Idx<Block>],
        flow: &Flow,
        mut at: impl FnMut(Idx<Inst>, &BitSet),
    ) {
        let mut live = self.live_set();
        for (bi, &b) in scope.iter().enumerate() {
            self.block_end(flow, bi, &mut live);
            self.walk_block(bi, &func.blocks[b].insts, &mut live, |_, id, live| {
                at(id, live)
            });
        }
    }
}

/// Flatten each statement's folded operands once. Later passes can fold more
/// values, so consumers expand any newly folded leaf at its use.
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
                nested => nested.uses_each(&mut |u| operand(func, inline, u, leaves)),
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
                inst.cell_reads_each(&mut |c| reads.leaves.push(raw(c) as u32 | CELL_LEAF));
                inst.uses_each(&mut |u| operand(func, inline, u, &mut reads.leaves));
                reads.stmt[raw(id)] = (start, reads.leaves.len() as u32);
            }
            let start = reads.leaves.len() as u32;
            block
                .term
                .uses_each(&mut |u| operand(func, inline, u, &mut reads.leaves));
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

    fn values(func: &MirFunc, inline: &[bool], leaves: &[u32], mut push: impl FnMut(Idx<Inst>)) {
        for &leaf in leaves {
            if leaf & CELL_LEAF == 0 {
                register_reads(
                    func,
                    inline,
                    Idx::from_raw(RawIdx::from_u32(leaf)),
                    &mut push,
                );
            }
        }
    }

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
            cell_reads(
                func,
                inline,
                alias,
                Idx::from_raw(RawIdx::from_u32(leaf)),
                &mut push,
            );
        }
    }
}

/// The loads in `scope` that can render as their cell at every use: no
/// statement writes the cell while the loaded value is live. `pinned` cells
/// are excluded.
pub(crate) fn stable_loads(
    func: &MirFunc,
    scope: &[Idx<Block>],
    flow: &Flow,
    inline: &[bool],
    pinned: &[bool],
    loops: &HashMap<Idx<Block>, ForLoop>,
    reads: &Reads,
) -> Vec<Idx<Inst>> {
    let mut written = vec![false; func.cells.len()];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            func.insts[id].cell_writes_each(&mut |c| written[raw(c)] = true);
        }
    }
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
    let by_cell = Chains::new(func.cells.len(), loads.iter().map(|&(_, cell)| raw(cell)));
    let tracked = |v: Idx<Inst>, bits: &mut Vec<u32>| {
        if li[raw(v)] != u32::MAX {
            bits.push(li[raw(v)]);
        }
    };
    let liveness = Entries::new(
        func,
        scope,
        flow,
        loads.len(),
        |_, id, bits| {
            if !inline[raw(id)] {
                tracked(id, bits)
            }
        },
        |_, id, bits| {
            if !inline[raw(id)] {
                Reads::values(func, inline, reads.stmt(id), |v| tracked(v, bits))
            }
        },
        |_, b, bits| Reads::values(func, inline, reads.term(b), |v| tracked(v, bits)),
    );
    // A statement writes its cells after it reads its operands, so a write
    // conflicts with a load that is still live after the statement.
    let mut conflict = vec![false; loads.len()];
    // A loop counter is a fresh Lua local; a load surviving an early exit
    // must stay in a register.
    for (&header, cl) in loops {
        let ForControl::Numeric { slot, .. } = cl.control else {
            continue;
        };
        for k in by_cell.of(raw(slot)) {
            conflict[k] = scope.iter().enumerate().any(|(bi, &b)| {
                b != header
                    && b != cl.latch
                    && !cl.region.contains(&b)
                    && liveness.live_in.get(bi, k)
            });
        }
    }
    liveness.walk(func, scope, flow, |id, live| {
        if inline[raw(id)] {
            return;
        }
        func.insts[id].cell_writes_each(&mut |cell| {
            for k in by_cell.of(raw(cell)) {
                conflict[k] |= live.get(k);
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
    let mut ci = vec![u32::MAX; func.cells.len()];
    let mut c_count = 0u32;
    for &c in &cells {
        if ci[raw(c)] == u32::MAX {
            ci[raw(c)] = k_count as u32 + c_count;
            c_count += 1;
        }
    }
    let by_cell = Chains::new(
        c_count as usize,
        cells.iter().map(|&c| ci[raw(c)] as usize - k_count),
    );
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
    let liveness = Entries::new(
        func,
        scope,
        flow,
        k_count + c_count as usize,
        |_, id, bits| {
            if !inline[raw(id)] {
                value_bit(id, bits);
                func.insts[id].cell_writes_each(&mut |c| cell_bit(c, bits));
            }
        },
        |_, id, bits| {
            if !inline[raw(id)] {
                Reads::values(func, inline, reads.stmt(id), |v| value_bit(v, bits));
                Reads::cells(func, inline, &[], reads.stmt(id), |c| cell_bit(c, bits));
            }
        },
        |_, b, bits| {
            Reads::values(func, inline, reads.term(b), |v| value_bit(v, bits));
            Reads::cells(func, inline, &[], reads.term(b), |c| cell_bit(c, bits));
        },
    );
    let mut conflict = vec![false; k_count];
    liveness.walk(func, scope, flow, |id, live| {
        if inline[raw(id)] {
            return;
        }
        func.insts[id].cell_writes_each(&mut |c| {
            if ci[raw(c)] == u32::MAX {
                return;
            }
            let copies = match func.insts[id] {
                Inst::Store { value, .. } => Some(value),
                _ => None,
            };
            for k in by_cell.of(ci[raw(c)] as usize - k_count) {
                if live.get(k) && copies != Some(vals[k]) {
                    conflict[k] = true;
                }
            }
        });
        if vi[raw(id)] != u32::MAX {
            let k = vi[raw(id)] as usize;
            let bit = ci[raw(cells[k])] as usize;
            if live.get(bit) || by_cell.of(bit - k_count).any(|o| o != k && live.get(o)) {
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
fn result_discarded(func: &MirFunc, inline: &[bool], readers: &[u32], id: Idx<Inst>) -> bool {
    readers[raw(id)] == 0
        && match func.insts[id] {
            Inst::Call { .. } => true,
            Inst::CallValue { callee, .. } => {
                !inline[raw(callee)] || matches!(func.insts[callee], Inst::Load(_))
            }
            _ => false,
        }
}

pub(crate) struct ValueAlloc {
    /// Dense `raw(inst) -> color` (`u32::MAX` = uncolored: inlined or non-value).
    pub colors: Vec<u32>,
    /// Loop-local colors let LuaJIT prune temps from exit snapshots; inherited
    /// floating-point values can otherwise abort side-trace register coalescing.
    pub color_scope: Vec<Option<Idx<Block>>>,
}

pub(crate) fn cell_defs(inst: &Inst, push: &mut impl FnMut(Idx<Cell>)) {
    // Residual bound-call destinations are placeholders, not emitted locals.
    if !matches!(inst, Inst::CallBoundMulti { .. }) {
        inst.cell_writes_each(push);
    }
}

/// The cell a value is computed in instead of a register of its own (see
/// [`store_targets`]); an empty table means none is.
pub(crate) fn alias_at(alias: &[Option<Idx<Cell>>], v: Idx<Inst>) -> Option<Idx<Cell>> {
    alias.get(raw(v)).copied().flatten()
}

/// Counting a value computed in a cell.
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

/// Allocate registers and cells in separate color spaces over shared liveness.
/// Expand folded operands into the reads they perform when emitted.
pub(crate) struct Locals<'a> {
    func: &'a MirFunc,
    scope: &'a [Idx<Block>],
    flow: &'a Flow,
    inline: &'a [bool],
    values: Vec<Idx<Inst>>,
    /// Logical cell index -> cell. A compact inline template leaves holes in
    /// the logical source arena; they stay as zero-edge virtual cells so
    /// removing them does not renumber later colors.
    cells: Vec<Option<Idx<Cell>>>,
    ci: Vec<u32>,
    /// Words of a bit row that hold registers; cells follow.
    v_words: usize,
    entries: Entries,
    value_graph: Neighbors,
    cell_graph: Neighbors,
    /// By logical cell index: the cell owns a color alone.
    force: Vec<bool>,
}

impl<'a> Locals<'a> {
    /// `pinned` and `extra_pinned` cells never share a color; the second
    /// names cells the emitter references implicitly (a coroutine's frame
    /// parameter). `readers` counts the operands in `scope` that read each
    /// value.
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
        mut readers: Vec<u32>,
    ) -> Self {
        let n = scope.len();
        // Dropped instructions are not readers; out-of-scope blocks are still placed.
        if func.blocks.len() != n {
            for (b, block) in func.blocks.iter() {
                if flow.index(b).is_none() {
                    for &id in &block.insts {
                        func.insts[id].uses_each(&mut |v| readers[raw(v)] += 1);
                    }
                    block.term.uses_each(&mut |v| readers[raw(v)] += 1);
                }
            }
        }
        let mut values: Vec<Idx<Inst>> = Vec::new();
        for &b in scope {
            for &id in &func.blocks[b].insts {
                let inst = &func.insts[id];
                if inst.produces_value()
                    && !inline[raw(id)]
                    && alias_at(alias, id).is_none()
                    && !result_discarded(func, inline, &readers, id)
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

        let c_count = func.emit_cell_span() as usize;
        let mut cells: Vec<Option<Idx<Cell>>> = vec![None; c_count];
        let mut ci = vec![u32::MAX; func.cells.len()];
        for (id, _) in func.cells.iter() {
            let i = func.emit_cell_id(id);
            cells[i as usize] = Some(id);
            ci[raw(id)] = i;
        }
        let bit_count = cell_base as usize + c_count;

        // A folded statement keeps only its cell writes: the suppressed store
        // of a forwarded tail call still clobbers its cell's color.
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
        let entries = Entries::new(
            func,
            scope,
            flow,
            bit_count,
            |_, id, bits| {
                if vi[raw(id)] != u32::MAX {
                    bits.push(vi[raw(id)]);
                }
                emitted_cell_defs(func, alias, id, &mut |c| bits.push(cell_base + ci[raw(c)]));
            },
            |_, id, bits| {
                if !inline[raw(id)] {
                    push_reads(bits, reads.stmt(id));
                }
            },
            |_, b, bits| push_reads(bits, reads.term(b)),
        );

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
        // A forced cell owns a color below every shared one, so its edges
        // never change a coloring.
        let mut allowed = BitSet::new(c_count);
        for (i, &force) in force.iter().enumerate() {
            if !force {
                allowed.set(i);
            }
        }

        let mut value_graph = Interference::new(v_count);
        let mut cell_graph = Interference::new(c_count);
        let mut live = entries.live_set();
        for (bi, &b) in scope.iter().enumerate() {
            entries.block_end(flow, bi, &mut live);
            entries.walk_block(bi, &func.blocks[b].insts, &mut live, |e, _, live| {
                for &d in entries.defs(e) {
                    if d < cell_base {
                        value_graph.add_live(d as usize, &live.words[..v_words], None);
                    } else if !force[(d - cell_base) as usize] {
                        cell_graph.add_live(
                            (d - cell_base) as usize,
                            &live.words[v_words..],
                            Some(&allowed.words),
                        );
                    }
                }
            });
            // Params are live from entry.
            if b == func.entry() {
                for &p in &func.params {
                    live.set((cell_base + ci[raw(p)]) as usize);
                }
                for &p in &func.params {
                    let pi = ci[raw(p)] as usize;
                    if force[pi] {
                        continue;
                    }
                    cell_graph.add_live(pi, &live.words[v_words..], Some(&allowed.words));
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
            entries,
            value_graph: value_graph.finish(v_count),
            cell_graph: cell_graph.finish(c_count),
            force,
        }
    }

    /// Assign register colors from the innermost loop that contains their live range.
    /// Separate palettes keep loop locals from shadowing outer locals.
    pub(crate) fn color_values(&self, loops: &[LoopScope]) -> ValueAlloc {
        let (func, blocks) = (self.func, self.scope);
        let n = blocks.len();
        let v_count = self.values.len();
        let value_bits = |rows: &BitRows, bi: usize, f: &mut dyn FnMut(usize)| {
            for_each_bit(&rows.row(bi)[..self.v_words], f)
        };

        // Values live across a loop back-edge cannot use a body local, which
        // Lua would reinitialize on every iteration.
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
                value_bits(&self.entries.def_set, bi, &mut fold);
                value_bits(&self.entries.use_set, bi, &mut fold);
            }
            for (k, ls) in loops.iter().enumerate() {
                let Some(hbi) = self.flow.index(ls.header) else {
                    continue;
                };
                let lp = k as u32 + 1;
                value_bits(&self.entries.live_in, hbi, &mut |v| {
                    let mut p = val_part[v];
                    if p == u32::MAX {
                        return;
                    }
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

        // Every color belongs to exactly one partition.
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
        // Pinned cells own their colors because closures capture them by identity.
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

        // Prefer a copy partner when it does not interfere, so the copy can disappear.
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
            func.insts[id].cell_reads_each(&mut |c| pinned[raw(c)] = true);
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

/// One emitted Lua local: a value register `r<color>` or a cell local `c<color>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KillLocal {
    Value(u32),
    Cell(u32),
}

impl KillLocal {
    /// Kill slots number the value colors first, then the cell colors.
    fn of_slot(slot: usize, value_count: u32) -> Self {
        if (slot as u32) < value_count {
            KillLocal::Value(slot as u32)
        } else {
            KillLocal::Cell(slot as u32 - value_count)
        }
    }
}

/// Nil kills make dead locals constants in LuaJIT exit snapshots, reducing
/// the parent-frame slots side traces need to inherit.
#[derive(Default)]
pub(crate) struct NilKills<'a> {
    /// Dense `raw(inst) -> range of after_locals`: the locals dead right after
    /// that instruction's statement.
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
        let Some(flow) = self.flow else {
            return out;
        };
        let (Some(fi), Some(ti)) = (flow.index(from), flow.index(to)) else {
            return out;
        };
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
                let local = KillLocal::of_slot(slot, self.value_count);
                if self.allowed_at(local, to) && !first_pure.contains(&slot) {
                    out.push(local);
                }
                bits &= bits - 1;
            }
        }
        out
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
            | Inst::VecIndexSlot(_)
            | Inst::NbIndexSlot(_)
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
    inst.uses_each(&mut |u| {
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

    let value_slot = |v: Idx<Inst>| -> Option<u32> {
        let c = value_colors[raw(v)];
        (c != u32::MAX).then_some(c)
    };
    let cell_slots = |c: Idx<Cell>, slots: &mut Vec<u32>| {
        if cell_slot[raw(c)] != u32::MAX {
            slots.push(cell_slot[raw(c)]);
        }
    };
    let leaf_slots = |leaves: &[u32], slots: &mut Vec<u32>| {
        Reads::values(func, inline, leaves, |v| slots.extend(value_slot(v)));
        Reads::cells(func, inline, alias, leaves, |c| cell_slots(c, slots));
    };
    // A folded statement defines no slot.
    let entries = Entries::new(
        func,
        scope,
        flow,
        s_count,
        |_, id, slots| {
            if !inline[raw(id)] {
                if func.insts[id].produces_value() {
                    slots.extend(value_slot(id));
                }
                emitted_cell_defs(func, alias, id, &mut |c| cell_slots(c, slots));
            }
        },
        |_, id, slots| {
            if !inline[raw(id)] {
                leaf_slots(reads.stmt(id), slots)
            }
        },
        |_, b, slots| leaf_slots(reads.term(b), slots),
    );

    let mut kills = NilKills {
        after_range: vec![(0, 0); func.insts.len()],
        after_locals: Vec::with_capacity(entries.bits.len()),
        entry_range: vec![(0, 0); func.blocks.len()],
        entry_locals: Vec::new(),
        live_in: BitRows::default(),
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

    // Skip a kill if a later straight-line statement overwrites the slot before
    // a call. LuaJIT prunes that slot when the continuation redefines it.
    let mut live = entries.live_set();
    let mut dead: Vec<usize> = Vec::new();
    let mut next_pure_defs: Vec<usize> = Vec::new();
    for (bi, &b) in scope.iter().enumerate() {
        entries.block_end(flow, bi, &mut live);
        kills.end_live.row_mut(bi).copy_from_slice(&live.words);
        next_pure_defs.clear();
        entries.walk_block(bi, &func.blocks[b].insts, &mut live, |e, id, live| {
            if inline[raw(id)] {
                return;
            }
            let defs = entries.defs(e);
            dead.clear();
            for &s in defs.iter().chain(entries.uses(e)) {
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
                kills.after_locals.push(KillLocal::of_slot(s, value_count));
            }
            kills.after_range[raw(id)] = (start, kills.after_locals.len() as u32);
        });
        // After the walk this holds the defs of the leading pure statements.
        let start = kills.first_pure_slots.len() as u32;
        kills.first_pure_slots.extend_from_slice(&next_pure_defs);
        kills.first_pure_range[bi] = (start, kills.first_pure_slots.len() as u32);
    }
    kills.live_in = entries.live_in;
    // Slots live at the end of a predecessor and dead on entry die on the edge;
    // the successor's label is the one place that covers every such edge.
    // A slot dies once per label, at the first edge that kills it.
    let mut entry_locals: Vec<KillLocal> = Vec::new();
    let mut seen = vec![0u64; kills.live_in.width];
    for (ti, &to) in scope.iter().enumerate() {
        let start = entry_locals.len() as u32;
        let dead_on_entry = kills.live_in.row(ti);
        seen.copy_from_slice(dead_on_entry);
        if to == func.entry() {
            // A parameter is defined at entry; one never read dies there.
            for &p in params {
                let s = cell_slot[raw(p)] as usize;
                if s != u32::MAX as usize && seen[s >> 6] & (1u64 << (s & 63)) == 0 {
                    seen[s >> 6] |= 1u64 << (s & 63);
                    entry_locals.push(KillLocal::of_slot(s, value_count));
                }
            }
        }
        let (first, end) = kills.first_pure_range[ti];
        let first_pure = &kills.first_pure_slots[first as usize..end as usize];
        for &fi in flow.preds(ti) {
            for (wi, &live) in kills.end_live.row(fi as usize).iter().enumerate() {
                let mut bits = live & !seen[wi];
                seen[wi] |= bits;
                while bits != 0 {
                    let slot = wi * 64 + bits.trailing_zeros() as usize;
                    let local = KillLocal::of_slot(slot, value_count);
                    if kills.allowed_at(local, to) && !first_pure.contains(&slot) {
                        entry_locals.push(local);
                    }
                    bits &= bits - 1;
                }
            }
        }
        kills.entry_range[raw(to)] = (start, entry_locals.len() as u32);
    }
    kills.entry_locals = entry_locals;
    kills
}
