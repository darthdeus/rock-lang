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

use rustc_hash::FxHashSet as HashSet;

use comfy_la_arena::Idx;

use crate::{collect_lambda_params, reachable_blocks_from, successors};
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
        BitSet { words: vec![0; n.div_ceil(64)] }
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
    fn copy_from(&mut self, o: &BitSet) {
        self.words.copy_from_slice(&o.words);
    }
    /// Zero every bit (reuse as a scratch buffer without reallocating).
    fn clear(&mut self) {
        self.words.iter_mut().for_each(|w| *w = 0);
    }
    /// `self |= o`; returns whether any bit was newly set.
    fn or_with(&mut self, o: &BitSet) -> bool {
        let mut changed = false;
        for (a, b) in self.words.iter_mut().zip(o.words.iter()) {
            let merged = *a | *b;
            changed |= merged != *a;
            *a = merged;
        }
        changed
    }
    fn for_each(&self, mut f: impl FnMut(usize)) {
        for (wi, &w) in self.words.iter().enumerate() {
            let mut bits = w;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                f(wi * 64 + b);
                bits &= bits - 1;
            }
        }
    }
}

/// Sparse symmetric adjacency in CSR form, built from an undirected edge list.
/// Replaces the dense `Interference` bit-matrix for value coloring: value live
/// ranges are short, so the interference graph is sparse and a dense `v^2`
/// matrix wastes its zeroing and its per-row coloring scan. Each input edge
/// `(a, b)` is stored in BOTH directions, so `neighbors(i)` yields every value
/// interfering with `i` regardless of discovery direction.
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

/// Raw arena index as a `usize` — the dense key for the `Vec` side-tables that
/// replace `Idx`-keyed `HashMap`s in the hot liveness/interference loops (an `Idx`
/// is dense over its arena, so a flat array beats any hash).
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
pub struct LoopScope {
    pub header: Idx<Block>,
    pub region: Vec<Idx<Block>>,
}

/// Result of [`color_values`]: the per-inst color table plus, per color, the
/// counted loop that lexically owns it (`None` = declared at function top).
pub struct ValueAlloc {
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

/// Color every value-producing instruction in `scope` (the reachable blocks of
/// one function or closure body) to a register index. Values that never
/// interfere share a color. Returns a dense `raw(inst) -> color` table
/// (`u32::MAX` = uncolored: inlined or non-value); callers render a colored
/// value as `r<color>`. Coloring is per-scope so a closure's values are
/// independent of the enclosing function's (only cells cross the boundary).
///
/// `inline` lists values the emitter tree-folds into their single use site (no
/// register): they are NOT colored, and a use of one is seen THROUGH to the
/// registers its expression reads (via [`effective_uses`]) so those stay live up
/// to the using instruction.
///
/// `loops` partitions the coloring: a value whose defs and effective uses all
/// sit inside one counted loop's body (and which is not live across the back
/// edge) is colored from that loop's own palette, and the color is reported in
/// [`ValueAlloc::color_scope`] so the emitter declares it inside the `for`
/// block. Palettes are disjoint, so a loop-scoped local never shares a name
/// with (or shadows) a function-top local.
pub fn color_values(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inline: &[bool],
    loops: &[LoopScope],
) -> ValueAlloc {
    let blocks: Vec<Idx<Block>> = scope.to_vec();
    let n = blocks.len();
    // Dense block -> scope-index (raw-id keyed; usize::MAX = not in this scope).
    let mut pos = vec![usize::MAX; func.blocks.len()];
    for (i, &b) in blocks.iter().enumerate() {
        pos[raw(b)] = i;
    }

    // Dense `0..V` index over the value-producing insts in this scope (excluding
    // inlined ones, which get no register), so liveness/interference run on bit
    // sets instead of per-element HashSets.
    let mut values: Vec<Idx<Inst>> = Vec::with_capacity(func.insts.len());
    for &b in &blocks {
        for &id in &func.blocks[b].insts {
            if func.insts[id].produces_value() && !inline[raw(id)] {
                values.push(id);
            }
        }
    }
    values.sort_by_key(|v| func.emit_inst_id(*v));
    values.dedup();
    let v_count = values.len();
    // Dense value -> dense index (raw-id keyed). `vi[raw(v)]` replaces the FxHash
    // lookup `vi[&v]` the liveness/interference inner loops did millions of times.
    let mut vi = vec![u32::MAX; func.insts.len()];
    for (i, &v) in values.iter().enumerate() {
        vi[raw(v)] = i as u32;
    }

    // Per-block upward-exposed uses and defs (bit sets). Inlined insts are skipped
    // (no def, no own-position use — their reads fold in at the use site).
    let mut use_set: Vec<BitSet> = vec![BitSet::new(v_count); n];
    let mut def_set: Vec<BitSet> = vec![BitSet::new(v_count); n];
    for (bi, &b) in blocks.iter().enumerate() {
        let block = &func.blocks[b];
        for &id in &block.insts {
            if inline[raw(id)] {
                continue;
            }
            effective_uses(func, inline, &func.insts[id], &mut |v| {
                let k = vi[raw(v)] as usize;
                if def_set[bi].words[k >> 6] & (1u64 << (k & 63)) == 0 {
                    use_set[bi].set(k);
                }
            });
            if func.insts[id].produces_value() {
                def_set[bi].set(vi[raw(id)] as usize);
            }
        }
        term_uses(&block.term, &mut |v| {
            // term operands may be inlined too -> see through them.
            let mut mark = |w: Idx<Inst>| {
                let k = vi[raw(w)] as usize;
                if def_set[bi].words[k >> 6] & (1u64 << (k & 63)) == 0 {
                    use_set[bi].set(k);
                }
            };
            if inline[raw(v)] {
                effective_uses(func, inline, &func.insts[v], &mut mark);
            } else {
                mark(v);
            }
        });
    }

    // Backward liveness fixpoint (in = use ∪ (out \ def)).
    // Only `live_in` is stored; `live_out[b] = ∪ successors' live_in` is
    // recomputed on the fly (in the fixpoint and again for the interference
    // seed), so the n-wide `live_out` bit-array is never allocated. The fixpoint
    // converges on `live_in` growth alone (monotone), so dropping the explicit
    // `live_out` change-tracking is correct.
    let mut live_in: Vec<BitSet> = vec![BitSet::new(v_count); n];
    // Scratch buffers reused across every block/iteration (the per-block `clone`
    // + fresh `out` alloc were the regalloc memmove/malloc hot spot).
    let mut out = BitSet::new(v_count);
    let mut inn = BitSet::new(v_count);
    let mut changed = true;
    while changed {
        changed = false;
        for bi in (0..n).rev() {
            let b = blocks[bi];
            out.clear();
            successors(&func.blocks[b].term, |s| {
                let si = pos[raw(s)];
                if si != usize::MAX {
                    out.or_with(&live_in[si]);
                }
            });
            inn.copy_from(&use_set[bi]);
            for (wi, (&o, &d)) in out.words.iter().zip(def_set[bi].words.iter()).enumerate() {
                inn.words[wi] |= o & !d;
            }
            changed |= live_in[bi].or_with(&inn);
        }
    }

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
        if p == 0 { 0 } else { block_part[raw(loops[p as usize - 1].header)] }
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
            def_set[bi].for_each(&mut fold);
            use_set[bi].for_each(&mut fold);
        }
        for (k, ls) in loops.iter().enumerate() {
            let hbi = pos[raw(ls.header)];
            if hbi == usize::MAX {
                continue;
            }
            let lp = k as u32 + 1;
            live_in[hbi].for_each(|v| {
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

    // Interference edges, collected from a single backward walk. Value live
    // ranges are short (mostly single-block), so the graph is SPARSE
    // (~0.2x the words a dense v_count^2 bit-matrix would touch); a CSR adjacency
    // avoids both the O(v^2) zeroing and the per-value full-row scan the dense
    // matrix paid in coloring, with much better cache behavior on the large
    // inlined functions that dominate. Each undirected pair is discovered exactly
    // once (at the later-defined endpoint's removal from `live`), so the edge set
    // has no duplicates and the resulting coloring is identical to the dense one.
    let mut edges: Vec<(u32, u32)> = Vec::new();
    let mut live = BitSet::new(v_count);
    for &b in blocks.iter() {
        let block = &func.blocks[b];
        // live-out = ∪ successors' live-in (recomputed; no stored live_out array).
        live.clear();
        successors(&block.term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                live.or_with(&live_in[si]);
            }
        });
        term_uses(&block.term, &mut |v| {
            // term operands are after all insts; see through inlined ones.
            if inline[raw(v)] {
                effective_uses(func, inline, &func.insts[v], &mut |w| live.set(vi[raw(w)] as usize));
            } else {
                live.set(vi[raw(v)] as usize);
            }
        });
        for &id in block.insts.iter().rev() {
            if inline[raw(id)] {
                continue; // no register; its reads fold in at the use site
            }
            if func.insts[id].produces_value() {
                let d = vi[raw(id)] as u32;
                live.remove(d as usize);
                live.for_each(|o| edges.push((d, o as u32)));
            }
            effective_uses(func, inline, &func.insts[id], &mut |v| live.set(vi[raw(v)] as usize));
        }
    }
    let adj = Csr::from_edges(&edges, v_count);

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
    for (i, &v) in values.iter().enumerate() {
        used.clear();
        for &nb in adj.neighbors(i) {
            let c = color_of[nb as usize];
            if c != u32::MAX {
                let c = c as usize;
                if c >= used.len() {
                    used.resize(c + 1, false);
                }
                used[c] = true;
            }
        }
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
        .map(|&p| if p == 0 { None } else { Some(loops[p as usize - 1].header) })
        .collect();
    ValueAlloc { colors, color_scope }
}

// --- cell coloring (mutable locals) ----------------------------------------
//
// Cells are mutable variables (Lua locals): `Store`/`CallMulti` dest/`MapNext`
// write = a def, `Load`/`MapNext` cursor = a use, a param is defined at entry.
// This is the classic live-variable interference (multiple defs, unlike SSA
// values), so two cells whose live ranges are disjoint share one `c<color>`.
// Without this each cell is its own local and a big function blows luajit's
// 200-local parse cap (a large update function can need 193 cells).

/// Cells an instruction DEFINES (writes).
pub(crate) fn cell_defs(inst: &Inst, push: &mut impl FnMut(Idx<Cell>)) {
    match inst {
        Inst::Store { cell, .. } => push(*cell),
        Inst::CallMulti { dests, .. } => dests.iter().for_each(|d| push(*d)),
        Inst::CallValueMulti { dests, .. } => dests.iter().for_each(|d| push(*d)),
        Inst::MapNext { key_cell, val_cell, .. } => {
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

fn inlined_operand_cell_uses(
    func: &MirFunc,
    inline: &[bool],
    inst: &Inst,
    push: &mut impl FnMut(Idx<Cell>),
) {
    inst_uses(inst, &mut |value| {
        if !inline[raw(value)] {
            return;
        }
        match &func.insts[value] {
            Inst::Load(cell) => push(*cell),
            nested => inlined_operand_cell_uses(func, inline, nested, push),
        }
    });
}

fn emitted_cell_uses(
    func: &MirFunc,
    inline: Option<&[bool]>,
    id: Idx<Inst>,
    push: &mut impl FnMut(Idx<Cell>),
) {
    if inline.is_some_and(|inline| inline[raw(id)]) {
        return;
    }
    cell_uses(&func.insts[id], push);
    if let Some(inline) = inline {
        inlined_operand_cell_uses(func, inline, &func.insts[id], push);
    }
}

fn term_cell_uses(
    func: &MirFunc,
    inline: Option<&[bool]>,
    term: &Term,
    push: &mut impl FnMut(Idx<Cell>),
) {
    let Some(inline) = inline else { return };
    term_uses(term, &mut |value| {
        if !inline[raw(value)] {
            return;
        }
        match &func.insts[value] {
            Inst::Load(cell) => push(*cell),
            nested => inlined_operand_cell_uses(func, inline, nested, push),
        }
    });
}

/// Color the cells of a function that has no emitter-only cell uses. Returns a
/// dense `raw(cell) -> color` table (`u32::MAX` = uncolored). Non-interfering
/// cells share a color; the caller renders a colored cell as `c<color>`. Cells
/// in detached nested CFGs are forced to a unique color and are never emitted
/// with their enclosing body.
pub fn color_cells(func: &MirFunc) -> Vec<u32> {
    color_cells_with_inlined_loads(func, None)
}

/// Cells held by reference for the whole body -- a closure's params, any cell
/// a detached (closure) sub-CFG touches, and cells the emit-id table forces --
/// as a dense `raw(cell) -> bool`. They never share a color and are never
/// nil-killed: the closure reads the local by identity, so a kill would be
/// visible through it.
pub(crate) fn pinned_cells(func: &MirFunc) -> Vec<bool> {
    let top_set: HashSet<Idx<Block>> =
        reachable_blocks_from(func, func.entry()).into_iter().collect();
    let mut pinned = vec![false; func.cells.len()];
    for c in collect_lambda_params(func) {
        pinned[raw(c)] = true;
    }
    for (bid, b) in func.blocks.iter() {
        if top_set.contains(&bid) {
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

pub(crate) fn color_cells_with_inlined_loads(func: &MirFunc, inline: Option<&[bool]>) -> Vec<u32> {
    color_cells_with_inlined_loads_and_pinned_cells(func, inline, &[])
}

/// Like [`color_cells_with_inlined_loads`], with additional cells that the
/// emitter uses implicitly and therefore cannot see in ordinary MIR liveness.
pub(crate) fn color_cells_with_inlined_loads_and_pinned_cells(
    func: &MirFunc,
    inline: Option<&[bool]>,
    extra_pinned: &[Idx<Cell>],
) -> Vec<u32> {
    let top: Vec<Idx<Block>> = reachable_blocks_from(func, func.entry());
    let n = top.len();
    // Dense block -> top-index (raw-id keyed; usize::MAX = not a top block).
    let mut pos = vec![usize::MAX; func.blocks.len()];
    for (i, &b) in top.iter().enumerate() {
        pos[raw(b)] = i;
    }

    let mut pinned = pinned_cells(func);
    for &cell in extra_pinned {
        pinned[raw(cell)] = true;
    }
    // Forced emit ids may name a hole in a compact template's arena; such a
    // hole still consumes a unique color so later colors keep their numbers.
    let forced_emit_ids: HashSet<u32> = func
        .emit_ids
        .as_ref()
        .map(|ids| ids.forced_cells.iter().copied().collect())
        .unwrap_or_default();

    // Color EVERY cell of the function (an ordinary fn has no frame cells) into a
    // dense `0..C` index space so liveness/interference run on bitsets. A compact
    // inline template leaves holes in the logical source arena; retain those as
    // zero-edge virtual cells so removing them does not renumber later colors.
    let c_count = func.emit_cell_span() as usize;
    let mut ordered: Vec<Option<Idx<Cell>>> = vec![None; c_count];
    for (id, _) in func.cells.iter() {
        ordered[func.emit_cell_id(id) as usize] = Some(id);
    }
    // Dense physical cell -> logical index (raw-id keyed); replaces the `ci[&c]`
    // FxHash lookups in the liveness/interference inner loops.
    let mut ci = vec![u32::MAX; func.cells.len()];
    for (i, c) in ordered.iter().enumerate() {
        if let Some(c) = c {
            ci[raw(*c)] = i as u32;
        }
    }

    // Per-block upward-exposed uses and defs (bit sets) over top-level blocks.
    let mut use_set: Vec<BitSet> = vec![BitSet::new(c_count); n];
    let mut def_set: Vec<BitSet> = vec![BitSet::new(c_count); n];
    for (bi, &b) in top.iter().enumerate() {
        for &id in &func.blocks[b].insts {
            // A MapNext both uses (cursor) and defs (key,val); process the use
            // first so the cursor read is upward-exposed.
            emitted_cell_uses(func, inline, id, &mut |c| {
                let k = ci[raw(c)] as usize;
                if def_set[bi].words[k >> 6] & (1u64 << (k & 63)) == 0 {
                    use_set[bi].set(k);
                }
            });
            cell_defs(&func.insts[id], &mut |c| def_set[bi].set(ci[raw(c)] as usize));
        }
        term_cell_uses(func, inline, &func.blocks[b].term, &mut |c| {
            let k = ci[raw(c)] as usize;
            if def_set[bi].words[k >> 6] & (1u64 << (k & 63)) == 0 {
                use_set[bi].set(k);
            }
        });
    }

    // Backward liveness via a WORKLIST (in = use ∪ (out \ def)). Cells cross
    // blocks and loops drove the old round-robin to many full re-scans (measured
    // up to 24 sweeps); the worklist re-processes only a block whose successor's
    // live-in grew, so its work tracks actual propagation. Only `live_in` is
    // stored; `live_out[b] = ∪ successors' live_in` is recomputed on the fly (here
    // and at the interference seed). Predecessors (within scope) drive re-queueing.
    let mut live_in: Vec<BitSet> = vec![BitSet::new(c_count); n];

    // Predecessor CSR over scope blocks: `pred_list[pred_off[si]..pred_off[si+1]]`
    // are the scope-indices that have `top[si]` as a successor.
    let mut pred_off = vec![0u32; n + 1];
    for &b in &top {
        successors(&func.blocks[b].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                pred_off[si + 1] += 1;
            }
        });
    }
    for i in 0..n {
        pred_off[i + 1] += pred_off[i];
    }
    let mut pred_list = vec![0u32; pred_off[n] as usize];
    let mut fill = pred_off.clone();
    for (bi, &b) in top.iter().enumerate() {
        successors(&func.blocks[b].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                pred_list[fill[si] as usize] = bi as u32;
                fill[si] += 1;
            }
        });
    }

    let mut out = BitSet::new(c_count);
    let mut inn = BitSet::new(c_count);
    // Seed every block, processed high-index-first (popping the end) so the first
    // sweep mirrors the old reverse-order pass before any re-queue.
    let mut work: Vec<u32> = (0..n as u32).collect();
    let mut in_queue = vec![true; n];
    while let Some(bi) = work.pop() {
        let bi = bi as usize;
        in_queue[bi] = false;
        let b = top[bi];
        out.clear();
        successors(&func.blocks[b].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                out.or_with(&live_in[si]);
            }
        });
        // in = use ∪ (out \ def): start from use, then add out-minus-def.
        inn.copy_from(&use_set[bi]);
        for (wi, (&o, &d)) in out.words.iter().zip(def_set[bi].words.iter()).enumerate() {
            inn.words[wi] |= o & !d;
        }
        if live_in[bi].or_with(&inn) {
            // live-in grew -> each predecessor's live-out (hence live-in) may grow.
            for pi in pred_off[bi]..pred_off[bi + 1] {
                let p = pred_list[pi as usize] as usize;
                if !in_queue[p] {
                    in_queue[p] = true;
                    work.push(p as u32);
                }
            }
        }
    }

    // Interference (symmetric) as a bit set per cell. Walk each block backward
    // maintaining the live bit set; a cell defined at a point interferes with
    // everything else live there. Force-unique cells get their own color below,
    // so we don't bother building their rows.
    // Dense force-unique flag (per dense cell index) — replaces the `force_idx`
    // HashSet lookups in the per-live-bit interference inner loop.
    let mut force_idx = vec![false; c_count];
    for (i, c) in ordered.iter().enumerate() {
        if let Some(c) = c {
            force_idx[i] = pinned[raw(*c)];
        }
    }
    for &emit_id in &forced_emit_ids {
        if let Some(slot) = force_idx.get_mut(emit_id as usize) {
            *slot = true;
        }
    }
    // Interference edges between NON-force-unique cells only: a force-unique cell
    // gets a dedicated color in `0..base` and every other cell's search starts at
    // `base`, so edges touching a force cell never change a coloring result. CSR
    // adjacency (sparse — short cell live ranges) replaces the dense `c_count^2`
    // bit-matrix. Duplicate edges (a cell with several defs) are harmless: they
    // mark the same neighbor color used. Edges between non-force cells reconstruct
    // exactly the matrix rows the coloring below consulted, so output is identical.
    let mut edges: Vec<(u32, u32)> = Vec::new();
    let push_live = |d: usize, live: &BitSet, edges: &mut Vec<(u32, u32)>| {
        if !force_idx[d] {
            live.for_each(|o| {
                if o != d && !force_idx[o] {
                    edges.push((d as u32, o as u32));
                }
            });
        }
    };
    let mut live = BitSet::new(c_count);
    let mut defs: Vec<usize> = Vec::new();
    for &b in top.iter() {
        let block = &func.blocks[b];
        // live-out = ∪ successors' live-in (recomputed; no stored live_out array).
        live.clear();
        successors(&block.term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                live.or_with(&live_in[si]);
            }
        });
        term_cell_uses(func, inline, &block.term, &mut |c| live.set(ci[raw(c)] as usize));
        for &id in block.insts.iter().rev() {
            defs.clear();
            cell_defs(&func.insts[id], &mut |c| defs.push(ci[raw(c)] as usize));
            for &d in &defs {
                push_live(d, &live, &mut edges);
            }
            for &d in &defs {
                live.remove(d);
            }
            emitted_cell_uses(func, inline, id, &mut |c| live.set(ci[raw(c)] as usize));
        }
        // Params are live from entry: interfere them with live-in to entry and
        // with one another.
        if b == func.entry() {
            for &p in &func.params {
                let pi = ci[raw(p)] as usize;
                push_live(pi, &live, &mut edges);
                if !force_idx[pi] {
                    for &q in &func.params {
                        let qi = ci[raw(q)] as usize;
                        if pi != qi && !force_idx[qi] {
                            edges.push((pi as u32, qi as u32));
                        }
                    }
                }
            }
        }
    }
    let adj = Csr::from_edges(&edges, c_count);

    // A force-unique cell conflicts with EVERY cell (a closure captures the local
    // by identity, so it must never be reused). Rather than materialize O(F x C)
    // interference edges, give each force-unique cell its own color 0..F-1 and
    // start every other cell's search at F — equivalent, but avoids the edge
    // explosion that once dominated this pass on inlined functions.
    // Dense `raw(cell) -> color` (`u32::MAX` = uncolored), so the emitter reads a
    // cell's color by array index instead of hashing per `Load`/`Store`.
    let mut colors: Vec<u32> = vec![u32::MAX; func.cells.len()];
    let mut color_of: Vec<u32> = vec![u32::MAX; c_count];
    let mut next_unique = 0u32;
    for (i, c) in ordered.iter().enumerate() {
        if force_idx[i] {
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
    let mut used: Vec<bool> = Vec::new();
    for (i, c) in ordered.iter().enumerate() {
        if force_idx[i] {
            continue;
        }
        used.clear();
        for &nb in adj.neighbors(i) {
            let col = color_of[nb as usize];
            if col != u32::MAX {
                let col = col as usize;
                if col >= used.len() {
                    used.resize(col + 1, false);
                }
                used[col] = true;
            }
        }
        let mut col = base as usize;
        while col < used.len() && used[col] {
            col += 1;
        }
        if let Some(c) = c {
            colors[raw(*c)] = col as u32;
        }
        color_of[i] = col as u32;
    }
    colors
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

pub(crate) struct NilKills {
    /// Dense `raw(inst) -> locals dead right after that instruction's statement`.
    pub after_inst: Vec<Vec<KillLocal>>,
    /// Dense `raw(block) -> locals dead on entry to the block that some
    /// predecessor still had live` (dying on the edge or at its terminator).
    pub at_entry: Vec<Vec<KillLocal>>,
    /// Per scope block: live-in slots, and slots live at the block's end
    /// (successor live-in plus the terminator's own reads).
    live_in: Vec<BitSet>,
    end_live: Vec<BitSet>,
    pos: Vec<usize>,
    /// Per scope block: slots redefined by its leading call-free statements,
    /// so an entry kill of them would be wasted.
    first_pure_defs: Vec<Vec<usize>>,
    value_count: u32,
    value_scope: Vec<Option<Idx<Block>>>,
    loop_blocks: Vec<(Idx<Block>, HashSet<Idx<Block>>)>,
}

impl NilKills {
    /// No kills anywhere (coroutine `__step` bodies, or the pass disabled).
    pub(crate) fn none(func: &MirFunc) -> Self {
        NilKills {
            after_inst: vec![Vec::new(); func.insts.len()],
            at_entry: vec![Vec::new(); func.blocks.len()],
            live_in: Vec::new(),
            end_live: Vec::new(),
            pos: Vec::new(),
            first_pure_defs: Vec::new(),
            value_count: 0,
            value_scope: Vec::new(),
            loop_blocks: Vec::new(),
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
        let (Some(&fi), Some(&ti)) = (self.pos.get(raw(from)), self.pos.get(raw(to))) else {
            return Vec::new();
        };
        if fi == usize::MAX || ti == usize::MAX {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (wi, (&e, &l)) in self.end_live[fi].words.iter().zip(self.live_in[ti].words.iter()).enumerate() {
            let mut bits = e & !l;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                let slot = wi * 64 + b;
                let local = self.decode(slot);
                if self.allowed_at(local, to) && !self.first_pure_defs[ti].contains(&slot) {
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
        Inst::Const(_) | Inst::Global(_) | Inst::Load(_) | Inst::Store { .. }
            | Inst::Field { .. } | Inst::TupleIndex { .. }
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::UintAdd(..) | Inst::UintSub(..) | Inst::UintMul(..) | Inst::UintDiv(..) | Inst::UintMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::UintLt(..) | Inst::UintLeq(..) | Inst::UintGt(..) | Inst::UintGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::FloatToInt(_) | Inst::IntToFloat(_) | Inst::UintToFloat(_) | Inst::FloatToF32(_) | Inst::IntToUint(_)
            | Inst::UintToInt(_) | Inst::FloatToUint(_) | Inst::UintBitcastInt(_) | Inst::IntBitcastUint(_)
            | Inst::Neg(_) | Inst::Not(_) | Inst::BitNot(_) | Inst::UintBitNot(_)
            | Inst::BitAnd(..) | Inst::BitOr(..) | Inst::BitXor(..) | Inst::Shl(..) | Inst::Shr(..)
            | Inst::UintBitAnd(..) | Inst::UintBitOr(..) | Inst::UintBitXor(..) | Inst::UintShl(..) | Inst::UintShr(..)
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
/// `inline` marks instructions that emit no statement of their own (tree-folded
/// values, hoisted constants); their reads are attributed to the consumer, as
/// the emitter renders them. `loops` supplies the blocks in which a loop-scoped
/// register may be killed (its `for` body plus the latch label, both inside the
/// `for` block). `cell_excluded` is every cell this body does NOT render as a
/// bare `c<color>` local, from the caller's own declaration predicate: a kill of
/// one of those would assign a GLOBAL of that name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nil_kills(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inline: &[bool],
    value_colors: &[u32],
    value_scope: &[Option<Idx<Block>>],
    loops: &[LoopScope],
    cell_colors: &[u32],
    cell_excluded: &[bool],
    params: &[Idx<Cell>],
) -> NilKills {
    let n = scope.len();
    let mut pos = vec![usize::MAX; func.blocks.len()];
    for (i, &b) in scope.iter().enumerate() {
        pos[raw(b)] = i;
    }
    let value_count = value_colors
        .iter()
        .filter(|&&c| c != u32::MAX)
        .map(|&c| c + 1)
        .max()
        .unwrap_or(0);
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
    let value_slot = |v: Idx<Inst>| -> Option<usize> {
        let c = value_colors[raw(v)];
        (c != u32::MAX).then_some(c as usize)
    };
    let uses_of = |id: Idx<Inst>, push: &mut dyn FnMut(usize)| {
        effective_uses(func, inline, &func.insts[id], &mut |v| {
            if let Some(s) = value_slot(v) {
                push(s);
            }
        });
        emitted_cell_uses(func, Some(inline), id, &mut |c| {
            let s = cell_slot[raw(c)];
            if s != u32::MAX {
                push(s as usize);
            }
        });
    };
    let defs_of = |id: Idx<Inst>, push: &mut dyn FnMut(usize)| {
        if func.insts[id].produces_value() {
            if let Some(s) = value_slot(id) {
                push(s);
            }
        }
        cell_defs(&func.insts[id], &mut |c| {
            let s = cell_slot[raw(c)];
            if s != u32::MAX {
                push(s as usize);
            }
        });
    };
    let term_slots = |term: &Term, push: &mut dyn FnMut(usize)| {
        term_uses(term, &mut |v| {
            if inline[raw(v)] {
                effective_uses(func, inline, &func.insts[v], &mut |w| {
                    if let Some(s) = value_slot(w) {
                        push(s);
                    }
                });
            } else if let Some(s) = value_slot(v) {
                push(s);
            }
        });
        term_cell_uses(func, Some(inline), term, &mut |c| {
            let s = cell_slot[raw(c)];
            if s != u32::MAX {
                push(s as usize);
            }
        });
    };

    // Per-block upward-exposed uses and defs, then the same worklist liveness
    // as `color_cells_with_inlined_loads`, over the slot space.
    let mut use_set: Vec<BitSet> = vec![BitSet::new(s_count); n];
    let mut def_set: Vec<BitSet> = vec![BitSet::new(s_count); n];
    for (bi, &b) in scope.iter().enumerate() {
        for &id in &func.blocks[b].insts {
            if inline[raw(id)] {
                continue;
            }
            uses_of(id, &mut |s| {
                if !def_set[bi].get(s) {
                    use_set[bi].set(s);
                }
            });
            defs_of(id, &mut |s| def_set[bi].set(s));
        }
        term_slots(&func.blocks[b].term, &mut |s| {
            if !def_set[bi].get(s) {
                use_set[bi].set(s);
            }
        });
    }
    let mut pred_off = vec![0u32; n + 1];
    for &b in scope {
        successors(&func.blocks[b].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                pred_off[si + 1] += 1;
            }
        });
    }
    for i in 0..n {
        pred_off[i + 1] += pred_off[i];
    }
    let mut pred_list = vec![0u32; pred_off[n] as usize];
    let mut fill = pred_off.clone();
    for (bi, &b) in scope.iter().enumerate() {
        successors(&func.blocks[b].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                pred_list[fill[si] as usize] = bi as u32;
                fill[si] += 1;
            }
        });
    }
    let mut live_in: Vec<BitSet> = vec![BitSet::new(s_count); n];
    let mut out = BitSet::new(s_count);
    let mut inn = BitSet::new(s_count);
    let mut work: Vec<u32> = (0..n as u32).collect();
    let mut in_queue = vec![true; n];
    while let Some(bi) = work.pop() {
        let bi = bi as usize;
        in_queue[bi] = false;
        out.clear();
        successors(&func.blocks[scope[bi]].term, |s| {
            let si = pos[raw(s)];
            if si != usize::MAX {
                out.or_with(&live_in[si]);
            }
        });
        inn.copy_from(&use_set[bi]);
        for (wi, (&o, &d)) in out.words.iter().zip(def_set[bi].words.iter()).enumerate() {
            inn.words[wi] |= o & !d;
        }
        if live_in[bi].or_with(&inn) {
            for pi in pred_off[bi]..pred_off[bi + 1] {
                let p = pred_list[pi as usize] as usize;
                if !in_queue[p] {
                    in_queue[p] = true;
                    work.push(p as u32);
                }
            }
        }
    }

    let mut kills = NilKills {
        after_inst: vec![Vec::new(); func.insts.len()],
        at_entry: vec![Vec::new(); func.blocks.len()],
        live_in,
        end_live: vec![BitSet::new(s_count); n],
        pos,
        first_pure_defs: vec![Vec::new(); n],
        value_count,
        value_scope: value_scope.to_vec(),
        loop_blocks: loops
            .iter()
            .map(|l| (l.header, l.region.iter().copied().collect()))
            .collect(),
    };

    // Backward walk per block: a slot read or written by a statement and not
    // live after it dies there -- unless a later statement of the block
    // redefines it before any statement that can call. No exit can then
    // observe the stale value: an exit at one of those pure statements
    // re-executes from it, and LuaJIT's own snapshot pruning drops a slot the
    // straight-line continuation defines. Such a kill would only cost the
    // interpreter a `KPRI`.
    let mut live = BitSet::new(s_count);
    let mut touched: Vec<usize> = Vec::new();
    let mut next_pure_defs: Vec<usize> = Vec::new();
    for (bi, &b) in scope.iter().enumerate() {
        let block = &func.blocks[b];
        live.clear();
        successors(&block.term, |s| {
            let si = kills.pos[raw(s)];
            if si != usize::MAX {
                live.or_with(&kills.live_in[si]);
            }
        });
        term_slots(&block.term, &mut |s| live.set(s));
        kills.end_live[bi].copy_from(&live);
        next_pure_defs.clear();
        for &id in block.insts.iter().rev() {
            if inline[raw(id)] {
                continue;
            }
            touched.clear();
            defs_of(id, &mut |s| touched.push(s));
            let n_defs = touched.len();
            uses_of(id, &mut |s| touched.push(s));
            let mut dead: Vec<usize> = Vec::new();
            for &s in &touched {
                if !live.get(s) && !dead.contains(&s) && !next_pure_defs.contains(&s) {
                    dead.push(s);
                }
            }
            if statement_is_pure(func, inline, id) {
                next_pure_defs.extend_from_slice(&touched[..n_defs]);
            } else {
                next_pure_defs.clear();
            }
            for &s in &dead {
                let local = kills.decode(s);
                kills.after_inst[raw(id)].push(local);
            }
            for &s in &touched[..n_defs] {
                live.remove(s);
            }
            for &s in &touched[n_defs..] {
                live.set(s);
            }
        }
        // After the walk this holds the defs of the leading pure statements.
        kills.first_pure_defs[bi] = next_pure_defs.clone();
        if b == func.entry() {
            // A parameter is defined at entry; one never read dies there.
            for &p in params {
                let s = cell_slot[raw(p)];
                if s != u32::MAX && !live.get(s as usize) {
                    let local = kills.decode(s as usize);
                    if !kills.at_entry[raw(b)].contains(&local) {
                        kills.at_entry[raw(b)].push(local);
                    }
                }
            }
        }
    }
    // Slots live at the end of a predecessor and dead on entry die on the edge;
    // the successor's label is the one place that covers every such edge.
    for &b in scope {
        successors(&func.blocks[b].term, |s| {
            for local in kills.on_edge(b, s) {
                if !kills.at_entry[raw(s)].contains(&local) {
                    kills.at_entry[raw(s)].push(local);
                }
            }
        });
    }
    kills
}

#[cfg(test)]
mod cell_color_tests {
    use super::*;
    use ustr::Ustr;
    use skull_mir::mir::Lit;

    /// Two cells whose live ranges do NOT overlap (`a` is read and dead before
    /// `b` is written) share one color.
    #[test]
    fn disjoint_cells_share_a_color() {
        let mut f = MirFunc::new(Ustr::from("t"));
        let e = f.entry();
        let a = f.new_cell(None);
        let b = f.new_cell(None);
        let one = f.emit(e, Inst::Const(Lit::Int(1)));
        f.emit(e, Inst::Store { cell: a, value: one });
        let la = f.emit(e, Inst::Load(a)); // a's last use -> dead after
        let na = f.emit(e, Inst::Neg(la));
        let two = f.emit(e, Inst::Const(Lit::Int(2)));
        f.emit(e, Inst::Store { cell: b, value: two }); // b born after a died
        let lb = f.emit(e, Inst::Load(b));
        let sum = f.emit(e, Inst::IntAdd(na, lb));
        f.set_term(e, Term::Return(vec![sum]));
        let colors = color_cells(&f);
        assert_eq!(colors[raw(a)], colors[raw(b)], "disjoint-lived cells should share a color");
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
        f.emit(e, Inst::Store { cell: a, value: one });
        let two = f.emit(e, Inst::Const(Lit::Int(2)));
        f.emit(e, Inst::Store { cell: b, value: two });
        let la = f.emit(e, Inst::Load(a));
        let lb = f.emit(e, Inst::Load(b));
        let sum = f.emit(e, Inst::IntAdd(la, lb)); // a and b both live here
        f.set_term(e, Term::Return(vec![sum]));
        let colors = color_cells(&f);
        assert_ne!(colors[raw(a)], colors[raw(b)], "simultaneously-live cells must not share a color");
    }
}
