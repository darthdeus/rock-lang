//! Scope ownership analysis for env-record lowering (P2 of
//! `docs/closure-env-records-plan.md`).
//!
//! MIR cells carry no scope: a lambda-local `let` and a mutated enclosing `var`
//! are both just `Store` targets in lambda blocks. To hoist a lambda body to a
//! top-level fn we must tell a lambda-local (declare it inside the hoisted body)
//! from a capture (thread it through the record). This computes, per emitted
//! `MirFunc`, which scope OWNS each cell and which cells each lambda scope
//! CAPTURES.
//!
//! The analysis is a pure fn over `MirFunc` and leans on one codegen fact: a
//! lambda's blocks are unreachable from the enclosing entry
//! (`reachable_blocks_from` follows terminators, never `Lambda` operands), so the
//! enclosing region and each lambda region are DISJOINT block sets -- the scope
//! tree is recoverable purely from block reachability.

use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::mir::*;

use skull_codegen_lua::reachable_blocks_from;

/// Index into [`ScopeTree::scopes`]. The enclosing fn is always [`ROOT`].
pub type ScopeId = usize;
/// The enclosing-function scope.
pub const ROOT: ScopeId = 0;

/// One lexical scope: the enclosing fn ([`ROOT`], `lambda == None`) or a lambda
/// sub-CFG. `region` is the blocks reachable from `entry`; regions are pairwise
/// disjoint. `parent` is the scope whose region holds the creating `Inst::Lambda`
/// (`None` only for root).
#[derive(Debug)]
pub struct Scope {
    pub entry: Idx<Block>,
    pub region: HashSet<Idx<Block>>,
    pub parent: Option<ScopeId>,
    /// The `Inst::Lambda` that creates this scope (`None` for the root fn).
    pub lambda: Option<Idx<Inst>>,
    pub params: Vec<Idx<Cell>>,
}

/// The scope tree of a `MirFunc`: root at [`ROOT`], one child per live lambda.
pub struct ScopeTree {
    pub scopes: Vec<Scope>,
    block_scope: HashMap<Idx<Block>, ScopeId>,
}

/// A cell's write location, collapsed to what the dominance rail needs: the
/// single writing inst's (block, within-block pos), or `Many` once a second write
/// is seen (a multi-write cell is never a by-value snapshot candidate). Absent
/// from [`Ownership::write_sites`] means zero writes.
#[derive(Clone, Copy, Debug)]
enum WriteSite {
    Once(Idx<Block>, usize),
    Many,
}

/// Cell ownership + per-scope captures. `owner[c]` is the lowest scope that must
/// declare `c` (the LCA of every scope accessing it); `captures[s]` are the cells
/// scope `s` must receive from an enclosing scope (owned strictly above `s`,
/// accessed at or below `s`). `writes`/`writing_scopes` are the static write
/// counts (`Store`, multi-return call `dests`, `MapNext` dests — the last count
/// double: one inst re-writes per iteration) feeding [`ScopeTree::boxed_cells`].
pub struct Ownership {
    pub owner: HashMap<Idx<Cell>, ScopeId>,
    /// Indexed by [`ScopeId`]; `captures[ROOT]` is always empty (root captures
    /// nothing).
    pub captures: Vec<HashSet<Idx<Cell>>>,
    pub writes: HashMap<Idx<Cell>, u32>,
    pub writing_scopes: HashMap<Idx<Cell>, HashSet<ScopeId>>,
    /// Loop-binding cells (counted-loop counter slots + `MapNext` key/val dests):
    /// capturing one is per-iteration BY VALUE, so it is excluded from
    /// [`ScopeTree::boxed_cells`] even though it is multi-store.
    pub loop_bindings: HashSet<Idx<Cell>>,
    /// Per-cell single-write site for the dominance rail, recorded in the SAME
    /// `accesses` walk that builds `writes` (no second arena rescan). Records all
    /// three write kinds the count walk sees (`Store`, `MapNext` dests, multi-
    /// return dests); [`Ownership::single_write_site`] queries it only for by-value
    /// capture candidates, which are never `MapNext` dests (those are loop bindings
    /// filtered before the rail).
    write_sites: HashMap<Idx<Cell>, WriteSite>,
}

impl Ownership {
    /// The block and within-block position of `c`'s single WRITE, or `None` if it
    /// is written other than exactly once (a multi-write cell is not a by-value
    /// candidate). A lookup into the site map recorded by [`ScopeTree::accesses`].
    pub fn single_write_site(&self, c: Idx<Cell>) -> Option<(Idx<Block>, usize)> {
        match self.write_sites.get(&c) {
            Some(WriteSite::Once(b, pos)) => Some((*b, *pos)),
            Some(WriteSite::Many) | None => None,
        }
    }
}

impl ScopeTree {
    pub fn build(func: &MirFunc) -> ScopeTree {
        let mut scopes = vec![Scope {
            entry: func.entry(),
            region: reachable_blocks_from(func, func.entry()).into_iter().collect(),
            parent: None,
            lambda: None,
            params: func.params.clone(),
        }];
        // block -> owning scope; regions are pairwise disjoint. Seeded with the
        // root region, then grown one live lambda at a time (below).
        let mut block_scope: HashMap<Idx<Block>, ScopeId> = HashMap::default();
        for &b in &scopes[ROOT].region {
            block_scope.insert(b, ROOT);
        }
        // Candidate lambdas: every `Inst::Lambda` still present in a block, WITH
        // its creating block (an inst lives in exactly one block). Inst-arena
        // order (deterministic ids).
        let mut lambda_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
        for (bid, b) in func.blocks.iter() {
            for &iid in &b.insts {
                if matches!(func.insts[iid], Inst::Lambda { .. }) {
                    lambda_block.insert(iid, bid);
                }
            }
        }
        let mut pending: Vec<(Idx<Inst>, Idx<Block>)> = func
            .insts
            .iter()
            .filter_map(|(lid, inst)| match inst {
                Inst::Lambda { .. } => lambda_block.get(&lid).map(|&b| (lid, b)),
                _ => None,
            })
            .collect();
        // A lambda is LIVE only if its creation site is REACHABLE -- its creating
        // block is owned by an already-live scope (root, or a live lambda).
        // fold_const_branches can drop the block holding an OUTER lambda's
        // creation while the outer's body blocks -- which create an INNER lambda
        // -- linger in the arena; the inner is then transitively dead (its parent
        // scope never exists). A fixpoint over the pending set adds a lambda once
        // its creating block enters `block_scope`, so a lambda whose parent chain
        // never becomes live is simply never added (no scope, no panic).
        let mut creating_block: Vec<Idx<Block>> = vec![func.entry()]; // root placeholder
        let mut grew = true;
        while grew {
            grew = false;
            let mut i = 0;
            while i < pending.len() {
                let (lid, cblock) = pending[i];
                if block_scope.contains_key(&cblock) {
                    let Inst::Lambda { params, entry } = &func.insts[lid] else { unreachable!() };
                    let sid = scopes.len();
                    let region: HashSet<Idx<Block>> = reachable_blocks_from(func, *entry).into_iter().collect();
                    for &b in &region {
                        let prev = block_scope.insert(b, sid);
                        debug_assert!(
                            prev.is_none(),
                            "scope regions must be pairwise disjoint (block in two scopes)"
                        );
                    }
                    scopes.push(Scope {
                        entry: *entry,
                        region,
                        parent: None, // filled below
                        lambda: Some(lid),
                        params: params.clone(),
                    });
                    creating_block.push(cblock);
                    pending.remove(i);
                    grew = true;
                } else {
                    i += 1;
                }
            }
        }
        // parent = the scope holding the block that creates this lambda. A live
        // scope's creating block entered `block_scope` before the scope was added,
        // so this indexing never fails (a lookup miss would be a hard error, not a
        // silent fallback -- the whole point of the reachability fixpoint above).
        for sid in 1..scopes.len() {
            let b = creating_block[sid];
            scopes[sid].parent = Some(block_scope[&b]);
        }
        ScopeTree { scopes, block_scope }
    }

    /// The scope owning block `b` (the scope whose region contains it).
    pub fn scope_of_block(&self, b: Idx<Block>) -> Option<ScopeId> {
        self.block_scope.get(&b).copied()
    }

    fn depth(&self, mut s: ScopeId) -> usize {
        let mut d = 0;
        while let Some(p) = self.scopes[s].parent {
            s = p;
            d += 1;
        }
        d
    }

    /// Lowest common ancestor of two scopes in the tree.
    fn lca(&self, mut a: ScopeId, mut b: ScopeId) -> ScopeId {
        let (mut da, mut db) = (self.depth(a), self.depth(b));
        while da > db {
            a = self.scopes[a].parent.unwrap();
            da -= 1;
        }
        while db > da {
            b = self.scopes[b].parent.unwrap();
            db -= 1;
        }
        while a != b {
            a = self.scopes[a].parent.unwrap();
            b = self.scopes[b].parent.unwrap();
        }
        a
    }

    /// For each cell: the scopes that touch it (any read or write, or a param of
    /// the scope), the static write count, and the writing scopes. Writes are
    /// `Store`, multi-return call `dests` (`CallMulti`/`CallValueMulti`/
    /// `CallBoundMulti` — raw lowering only dest-writes fresh temps, but the
    /// inliner rebinds dests onto surviving cells, so post-transform MIR can
    /// dest-write a captured cell), and `MapNext` dests (counted 2: one inst
    /// re-writes every iteration). (Terminators reference value insts, never
    /// cells, so inst-lists + params are complete.)
    #[allow(clippy::type_complexity)]
    fn accesses(
        &self,
        func: &MirFunc,
    ) -> (
        HashMap<Idx<Cell>, HashSet<ScopeId>>,
        HashMap<Idx<Cell>, u32>,
        HashMap<Idx<Cell>, HashSet<ScopeId>>,
        HashMap<Idx<Cell>, WriteSite>,
    ) {
        let mut acc: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        let mut writes: HashMap<Idx<Cell>, u32> = HashMap::default();
        let mut writing: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        let mut sites: HashMap<Idx<Cell>, WriteSite> = HashMap::default();
        for (sid, s) in self.scopes.iter().enumerate() {
            for &c in &s.params {
                acc.entry(c).or_default().insert(sid);
            }
            let mut write =
                |c: Idx<Cell>, n: u32, b: Idx<Block>, pos: usize, acc: &mut HashMap<Idx<Cell>, HashSet<ScopeId>>| {
                    acc.entry(c).or_default().insert(sid);
                    *writes.entry(c).or_insert(0) += n;
                    writing.entry(c).or_default().insert(sid);
                    sites
                        .entry(c)
                        .and_modify(|w| *w = WriteSite::Many)
                        .or_insert(WriteSite::Once(b, pos));
                };
            for &b in &s.region {
                for (pos, &iid) in func.blocks[b].insts.iter().enumerate() {
                    match &func.insts[iid] {
                        Inst::Load(c) => {
                            acc.entry(*c).or_default().insert(sid);
                        }
                        Inst::Store { cell, .. } => write(*cell, 1, b, pos, &mut acc),
                        Inst::MapNext { key_cell, val_cell, .. } => {
                            write(*key_cell, 2, b, pos, &mut acc);
                            write(*val_cell, 2, b, pos, &mut acc);
                        }
                        Inst::CallMulti { dests, .. }
                        | Inst::CallValueMulti { dests, .. }
                        | Inst::CallBoundMulti { dests, .. } => {
                            for &d in dests {
                                write(d, 1, b, pos, &mut acc);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (acc, writes, writing, sites)
    }

    /// Owner (LCA of accessors) + transitive captures for every cell.
    pub fn analyze(&self, func: &MirFunc) -> Ownership {
        let (acc, writes, writing_scopes, write_sites) = self.accesses(func);
        let mut owner: HashMap<Idx<Cell>, ScopeId> = HashMap::default();
        for (c, scopes) in &acc {
            let mut it = scopes.iter().copied();
            let first = it.next().expect("a cell has >= 1 accessor");
            let o = it.fold(first, |a, b| self.lca(a, b));
            owner.insert(*c, o);
        }
        // A cell is a capture of every scope STRICTLY BELOW its owner on a path to
        // an accessing scope: walk from each accessor up to (exclusive) the owner.
        // Intermediate scopes that never touch the cell still capture it (they
        // thread it to the record they construct) -- the nested-lambda case.
        let mut captures: Vec<HashSet<Idx<Cell>>> = vec![HashSet::default(); self.scopes.len()];
        for (c, scopes) in &acc {
            let o = owner[c];
            for &a in scopes {
                let mut s = a;
                while s != o {
                    captures[s].insert(*c);
                    s = self.scopes[s].parent.expect("owner is an ancestor of every accessor");
                }
            }
        }
        Ownership {
            owner,
            captures,
            writes,
            writing_scopes,
            loop_bindings: loop_binding_cells(func),
            write_sites,
        }
    }

    /// The set of captured cells that must be BOXED (`{ v = ... }`, by-reference)
    /// rather than captured by value. A captured cell boxes iff it can observe a
    /// write after the capture: it has >= 2 effective writes (the static write
    /// count from [`Ownership::writes`] plus 1 for a param's implicit entry
    /// store) OR it is written in a scope other than its owner (mutation from
    /// inside a lambda). A single-assignment captured cell (one effective write,
    /// never mutated from a non-owner) stays by-value -- boxing it would wrongly
    /// make a per-iteration capture see the final value.
    ///
    /// Loop-binding cells (FORL slots / `MapNext` dests) are never captured (the
    /// user binding is copied to a fresh single-assignment cell, and capturing a
    /// range counter defeats FORL recognition into a goto counter), so they never
    /// reach this set.
    ///
    /// The UNBOXED complement is not yet a validated by-value set: P2.6 (which
    /// snapshots it into record fields) must add the plan's dominance rail --
    /// the single write's block dominates the capturing `Inst::Lambda`'s block,
    /// else box.
    pub fn boxed_cells(&self, own: &Ownership) -> HashSet<Idx<Cell>> {
        let mut captured: HashSet<Idx<Cell>> = HashSet::default();
        for caps in &own.captures {
            captured.extend(caps.iter().copied());
        }
        let is_param: HashSet<Idx<Cell>> =
            self.scopes.iter().flat_map(|s| s.params.iter().copied()).collect();

        captured
            .into_iter()
            .filter(|c| {
                let effective = own.writes.get(c).copied().unwrap_or(0) + is_param.contains(c) as u32;
                let written_non_owner = own
                    .writing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != own.owner[c]));
                // A cell WRITTEN from inside a lambda boxes unconditionally -- the
                // mutation must be shared by reference. This precedes the
                // loop-binding exemption (R2): a lambda-written `for` binding is
                // LEGAL Rock (`for i in 0..3 { let f = () => { i = i + 10 } }`) --
                // the write steers the live counter, same as a plain in-loop write
                // and same as pre-arc inline closures -- so it boxes (shared), and
                // the per-iteration by-value exemption below applies only while NO
                // lambda writes the binding.
                if written_non_owner {
                    return true;
                }
                // A loop-binding cell captured (never lambda-written) is
                // per-iteration BY VALUE (each record snapshots that iteration's
                // value); it must not box even though it is multi-store.
                if own.loop_bindings.contains(c) {
                    return false;
                }
                effective >= 2
            })
            .collect()
    }
}

/// Loop-binding cells: the `for` desugarings' counter cells and `MapNext`
/// key/val dests, tagged `Cell::loop_binding` at lowering -- DESUGAR PROVENANCE,
/// not a structural guess. Capturing such a cell is per-iteration by value -- the
/// slot is a shared local re-assigned each iteration, so a record built inside
/// the loop snapshots THIS iteration's value. A user `while` counter or a
/// non-loop if-join `x = x + 1` (which the old structural latch detector misfired
/// on) has no tag, so it boxes -- the decided shared-binding semantics.
fn loop_binding_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    func.cells
        .iter()
        .filter(|(_, cell)| cell.loop_binding)
        .map(|(id, _)| id)
        .collect()
}

/// Dominance rail (docs/closure-env-records-plan.md, "Capture classification"):
/// a by-value capture snapshots the cell's value at the record's creation block,
/// which is only sound if the cell's single write is guaranteed to have run by
/// then. For a capture owned by the creating lambda's PARENT scope (a fresh local
/// read directly at the creation site — transitive captures arrive as a
/// parent-env field and are always available), require the write's block to
/// dominate the creation block; a non-dominating shape (a transform artifact,
/// never seen for well-formed lowering) falls back to a box. Returns the extra
/// cells to box.
///
/// The dominator machinery (`dominators`/`dominates`) lives in `skull-codegen-lua`
/// (shared with its relooper); this analysis reuses it so there is one dominator
/// implementation. The JS path passes `frame = None`.
pub fn non_dominating_by_value_captures(
    func: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    boxed: &HashSet<Idx<Cell>>,
    frame: Option<&skull_codegen_lua::FrameCtx>,
) -> HashSet<Idx<Cell>> {
    let mut extra: HashSet<Idx<Cell>> = HashSet::default();
    // block -> the `Inst::Lambda` creating a scope (its creation block).
    let mut create_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for (bid, b) in func.blocks.iter() {
        for &iid in &b.insts {
            if matches!(func.insts[iid], Inst::Lambda { .. }) {
                create_block.insert(iid, bid);
            }
        }
    }
    // Cache idoms per parent-scope entry (small regions; recomputed at most once
    // per distinct parent). Built LAZILY at the first cross-block dominance query:
    // most lambda scopes filter out every capture below (boxed / loop-binding /
    // frame / transitive / multi-write) or resolve same-block by inst position,
    // so the classic fixpoint is never needed for them -- computing it up front
    // per parent was pure waste.
    let mut idoms: HashMap<Idx<Block>, HashMap<Idx<Block>, Idx<Block>>> = HashMap::default();
    for s in &tree.scopes {
        let Some(lid) = s.lambda else { continue };
        let parent = s.parent.expect("a lambda scope has a parent");
        let Some(&cblock) = create_block.get(&lid) else { continue };
        let sid = tree.scope_of_block(cblock); // sanity: create block is in the parent region
        debug_assert_eq!(sid, Some(parent), "a lambda's creation block lies in its parent scope");
        let parent_entry = tree.scopes[parent].entry;
        // captures of THIS scope owned by the parent: read directly at cblock.
        let scope_id = (0..tree.scopes.len()).find(|&i| tree.scopes[i].lambda == Some(lid)).unwrap();
        for &c in &own.captures[scope_id] {
            if boxed.contains(&c) || own.loop_bindings.contains(&c) || extra.contains(&c) {
                continue;
            }
            // Frame cells are table-mediated (via `s` or a loop-binding
            // snapshot of a frame field) -- never box candidates.
            if frame.is_some_and(|f| f.fields.contains(&c)) {
                continue;
            }
            if own.owner[&c] != parent {
                continue; // transitive -> arrives via the parent env, always available
            }
            let Some((wblock, wpos)) = own.single_write_site(c) else {
                continue; // multi-write (already handled by boxed_cells) — skip
            };
            // Block granularity says a block dominates itself, but a same-block
            // write must still PRECEDE the lambda creation inst for the snapshot to
            // see it; compare inst positions when wblock == cblock.
            let write_dominates = if wblock == cblock {
                let cpos = func.blocks[cblock]
                    .insts
                    .iter()
                    .position(|&i| i == lid)
                    .expect("the creating Inst::Lambda lives in its creation block");
                wpos < cpos
            } else {
                let idom = idoms
                    .entry(parent_entry)
                    .or_insert_with(|| skull_codegen_lua::dominators(func, parent_entry));
                skull_codegen_lua::dominates(idom, wblock, cblock)
            };
            if !write_dominates {
                // Box fallback FIRST so correctness never depends on the assert
                // being compiled in (dead in release) -- keeps the fallback live in
                // dev instead of dead code after an abort.
                extra.insert(c);
                debug_assert!(false, "by-value capture write must dominate its creation site (dominance rail)");
                // Release: assert is compiled out, so surface the fallback (never
                // swallow) -- a non-dominating shape is a transform artifact worth
                // seeing.
                log::warn!(
                    "dominance rail: by-value capture cell {c:?} write does not dominate its creation site; boxing as fallback"
                );
            }
        }
    }
    extra
}
