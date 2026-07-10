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
/// accessed at or below `s`).
pub struct Ownership {
    pub owner: HashMap<Idx<Cell>, ScopeId>,
    /// Indexed by [`ScopeId`]; `captures[ROOT]` is always empty (root captures
    /// nothing).
    pub captures: Vec<HashSet<Idx<Cell>>>,
}

/// The block whose inst-list contains `iid` (a value-producing inst lives in
/// exactly one block). `None` if the inst was DCE'd out of every block.
fn block_of_inst(func: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Block>> {
    func.blocks.iter().find(|(_, b)| b.insts.contains(&iid)).map(|(bid, _)| bid)
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
        // A scope per LIVE lambda -- one whose creating inst is still in a block.
        // A DCE'd `Inst::Lambda` lingers in the arena but is never emitted, so it
        // gets no scope (and its `entry`'s blocks are unreachable anyway).
        let mut creating_block: Vec<Idx<Block>> = vec![func.entry()]; // root placeholder
        for (lid, inst) in func.insts.iter() {
            if let Inst::Lambda { params, entry } = inst {
                let Some(b) = block_of_inst(func, lid) else { continue };
                scopes.push(Scope {
                    entry: *entry,
                    region: reachable_blocks_from(func, *entry).into_iter().collect(),
                    parent: None, // filled below
                    lambda: Some(lid),
                    params: params.clone(),
                });
                creating_block.push(b);
            }
        }
        // block -> owning scope; regions are pairwise disjoint.
        let mut block_scope: HashMap<Idx<Block>, ScopeId> = HashMap::default();
        for (sid, s) in scopes.iter().enumerate() {
            for &b in &s.region {
                let prev = block_scope.insert(b, sid);
                debug_assert!(
                    prev.is_none(),
                    "scope regions must be pairwise disjoint (block in two scopes)"
                );
            }
        }
        // parent = the scope holding the block that creates this lambda.
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

    /// For each cell, the scopes that touch it: a `Load`/`Store`/`MapNext` dest in
    /// the scope's region, or a param of the scope. (Terminators reference value
    /// insts, never cells, so inst-lists + params are complete.)
    fn accessing_scopes(&self, func: &MirFunc) -> HashMap<Idx<Cell>, HashSet<ScopeId>> {
        let mut acc: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        for (sid, s) in self.scopes.iter().enumerate() {
            for &c in &s.params {
                acc.entry(c).or_default().insert(sid);
            }
            for &b in &s.region {
                for &iid in &func.blocks[b].insts {
                    match &func.insts[iid] {
                        Inst::Load(c) => {
                            acc.entry(*c).or_default().insert(sid);
                        }
                        Inst::Store { cell, .. } => {
                            acc.entry(*cell).or_default().insert(sid);
                        }
                        Inst::MapNext { key_cell, val_cell, .. } => {
                            acc.entry(*key_cell).or_default().insert(sid);
                            acc.entry(*val_cell).or_default().insert(sid);
                        }
                        _ => {}
                    }
                }
            }
        }
        acc
    }

    /// Owner (LCA of accessors) + transitive captures for every cell.
    pub fn analyze(&self, func: &MirFunc) -> Ownership {
        let acc = self.accessing_scopes(func);
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
        Ownership { owner, captures }
    }

    /// The set of captured cells that must be BOXED (`{ v = ... }`, by-reference)
    /// rather than captured by value. A captured cell boxes iff it can observe a
    /// write after the capture: it has >= 2 effective stores (an explicit
    /// `Inst::Store` count plus 1 for a param's implicit entry store) OR it is
    /// stored in a scope other than its owner (mutation from inside a lambda).
    /// A single-assignment captured cell (one effective store, never mutated from
    /// a non-owner) stays by-value -- boxing it would wrongly make a per-iteration
    /// capture see the final value.
    ///
    /// Loop-binding cells (FORL slots / `MapNext` dests) are never captured (the
    /// user binding is copied to a fresh single-assignment cell, and capturing a
    /// range counter defeats FORL recognition into a goto counter), so they never
    /// reach this set.
    pub fn boxed_cells(&self, func: &MirFunc, own: &Ownership) -> HashSet<Idx<Cell>> {
        let mut captured: HashSet<Idx<Cell>> = HashSet::default();
        for caps in &own.captures {
            captured.extend(caps.iter().copied());
        }
        let is_param: HashSet<Idx<Cell>> =
            self.scopes.iter().flat_map(|s| s.params.iter().copied()).collect();

        // Per-cell explicit store count + the set of scopes that store it.
        let mut stores: HashMap<Idx<Cell>, u32> = HashMap::default();
        let mut storing_scopes: HashMap<Idx<Cell>, HashSet<ScopeId>> = HashMap::default();
        for (sid, s) in self.scopes.iter().enumerate() {
            for &b in &s.region {
                for &iid in &func.blocks[b].insts {
                    if let Inst::Store { cell, .. } = &func.insts[iid] {
                        *stores.entry(*cell).or_insert(0) += 1;
                        storing_scopes.entry(*cell).or_default().insert(sid);
                    }
                }
            }
        }

        captured
            .into_iter()
            .filter(|c| {
                let effective = stores.get(c).copied().unwrap_or(0) + is_param.contains(c) as u32;
                let stored_non_owner = storing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != own.owner[c]));
                effective >= 2 || stored_non_owner
            })
            .collect()
    }
}
