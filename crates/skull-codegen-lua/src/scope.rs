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

use crate::reachable_blocks_from;

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
    ) {
        let mut acc: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        let mut writes: HashMap<Idx<Cell>, u32> = HashMap::default();
        let mut writing: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        for (sid, s) in self.scopes.iter().enumerate() {
            for &c in &s.params {
                acc.entry(c).or_default().insert(sid);
            }
            let mut write = |c: Idx<Cell>, n: u32, acc: &mut HashMap<Idx<Cell>, HashSet<ScopeId>>| {
                acc.entry(c).or_default().insert(sid);
                *writes.entry(c).or_insert(0) += n;
                writing.entry(c).or_default().insert(sid);
            };
            for &b in &s.region {
                for &iid in &func.blocks[b].insts {
                    match &func.insts[iid] {
                        Inst::Load(c) => {
                            acc.entry(*c).or_default().insert(sid);
                        }
                        Inst::Store { cell, .. } => write(*cell, 1, &mut acc),
                        Inst::MapNext { key_cell, val_cell, .. } => {
                            write(*key_cell, 2, &mut acc);
                            write(*val_cell, 2, &mut acc);
                        }
                        Inst::CallMulti { dests, .. }
                        | Inst::CallValueMulti { dests, .. }
                        | Inst::CallBoundMulti { dests, .. } => {
                            for &d in dests {
                                write(d, 1, &mut acc);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (acc, writes, writing)
    }

    /// Owner (LCA of accessors) + transitive captures for every cell.
    pub fn analyze(&self, func: &MirFunc) -> Ownership {
        let (acc, writes, writing_scopes) = self.accesses(func);
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
        Ownership { owner, captures, writes, writing_scopes, loop_bindings: loop_binding_cells(func) }
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
