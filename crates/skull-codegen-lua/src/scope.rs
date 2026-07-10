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
        // A scope per LIVE lambda -- one whose creating inst is still in a block.
        // A DCE'd `Inst::Lambda` lingers in the arena but is never emitted, so it
        // gets no scope (and its `entry`'s blocks are unreachable anyway). One
        // pass over block inst-lists finds each live lambda WITH its creating
        // block (an inst lives in exactly one block).
        let mut lambda_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
        for (bid, b) in func.blocks.iter() {
            for &iid in &b.insts {
                if matches!(func.insts[iid], Inst::Lambda { .. }) {
                    lambda_block.insert(iid, bid);
                }
            }
        }
        let mut creating_block: Vec<Idx<Block>> = vec![func.entry()]; // root placeholder
        for (lid, inst) in func.insts.iter() {
            if let Inst::Lambda { params, entry } = inst {
                let Some(&b) = lambda_block.get(&lid) else { continue };
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
                // A loop-binding cell captures per-iteration BY VALUE (each record
                // snapshots that iteration's value); it must not box even though it
                // is multi-store. See `loop_binding_cells`.
                if own.loop_bindings.contains(c) {
                    return false;
                }
                let effective = own.writes.get(c).copied().unwrap_or(0) + is_param.contains(c) as u32;
                let written_non_owner = own
                    .writing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != own.owner[c]));
                effective >= 2 || written_non_owner
            })
            .collect()
    }
}

/// Loop-binding cells: a counted-loop counter SLOT (target of a pure
/// `load slot; 1; slot+1; store slot` increment latch whose goto target is a
/// HEADER that tests `slot`) or a `MapNext` key/val dest. Capturing such a cell is
/// per-iteration by value -- in a goto-shaped loop the slot is a shared local
/// re-assigned each iteration, so a record built inside the loop snapshots THIS
/// iteration's value. (When the slot is NOT captured the emitter recognizes a real
/// Lua `for`; a direct capture defeats that recognition into a goto loop, which
/// this classifies.) The header-tests-slot guard is load-bearing: a plain
/// `x = x + 1` alone in a goto block matches the latch SHAPE but is a mutable var,
/// not a loop counter -- boxing it (not per-iteration) is required. Mirrors the
/// emitter's `latch_slot`/`match_header` pairing, scanned over MIR (pre-codegen).
fn loop_binding_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    let mut out: HashSet<Idx<Cell>> = HashSet::default();
    for (_, inst) in func.insts.iter() {
        if let Inst::MapNext { key_cell, val_cell, .. } = inst {
            out.insert(*key_cell);
            out.insert(*val_cell);
        }
    }
    for (_, b) in func.blocks.iter() {
        let Term::Goto(header) = b.term else { continue };
        if b.insts.len() != 4 {
            continue;
        }
        let (Inst::Load(slot), Inst::Const(Lit::Int(1)), Inst::IntAdd(a, bb), Inst::Store { cell, value }) =
            (&func.insts[b.insts[0]], &func.insts[b.insts[1]], &func.insts[b.insts[2]], &func.insts[b.insts[3]])
        else {
            continue;
        };
        if *a == b.insts[0] && *bb == b.insts[1] && *cell == *slot && *value == b.insts[2] && header_tests_slot(func, header, *slot) {
            out.insert(*slot);
        }
    }
    out
}

/// Does block `h` branch on a `slot < HI` / `slot <= HI` compare (a counted-loop
/// header whose counter is `slot`)? Mirrors the emitter's `match_header`.
fn header_tests_slot(func: &MirFunc, h: Idx<Block>, slot: Idx<Cell>) -> bool {
    let Term::Branch { cond, .. } = func.blocks[h].term else { return false };
    let (lhs, rhs) = match &func.insts[cond] {
        Inst::IntLt(a, b) | Inst::IntLeq(a, b) => (*a, *b),
        _ => return false,
    };
    let insts = &func.blocks[h].insts;
    let is_slot_load = |i: Idx<Inst>| matches!(&func.insts[i], Inst::Load(s) if *s == slot) && insts.contains(&i);
    is_slot_load(lhs) || is_slot_load(rhs)
}
