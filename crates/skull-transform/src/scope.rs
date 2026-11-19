//! Find the scope that owns each cell and the lambdas that capture it.
//! Lambda entries are reached through their creators, outside the enclosing CFG.

use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::mir::*;

use skull_mir::cfg::reachable_blocks_from;

pub(crate) type ScopeId = usize;
pub const ROOT: ScopeId = 0;

#[derive(Debug)]
pub struct Scope {
    pub entry: Idx<Block>,
    pub region: HashSet<Idx<Block>>,
    pub parent: Option<ScopeId>,
    /// The `Inst::Lambda` that creates this scope (`None` for the root fn).
    pub lambda: Option<Idx<Inst>>,
    pub params: Vec<Idx<Cell>>,
}

pub struct ScopeTree {
    pub scopes: Vec<Scope>,
    block_scope: HashMap<Idx<Block>, ScopeId>,
}

/// Write sites for checking whether initialization precedes capture.
#[derive(Clone, Copy, Debug)]
enum WriteSite {
    Once(Idx<Block>, usize),
    Many,
}

/// Ownership is the common ancestor of every scope that uses the cell.
pub struct Ownership {
    pub owner: HashMap<Idx<Cell>, ScopeId>,
    pub captures: Vec<HashSet<Idx<Cell>>>,
    pub writes: HashMap<Idx<Cell>, u32>,
    pub writing_scopes: HashMap<Idx<Cell>, HashSet<ScopeId>>,
    /// Loop-binding cells (counted-loop counter slots + `MapNext` key/val dests):
    /// capturing one is per-iteration BY VALUE, so it is excluded from
    /// [`ScopeTree::boxed_cells`] even though it is multi-store.
    pub loop_bindings: HashSet<Idx<Cell>>,
    write_sites: HashMap<Idx<Cell>, WriteSite>,
}

impl Ownership {
    pub(crate) fn single_write_site(&self, c: Idx<Cell>) -> Option<(Idx<Block>, usize)> {
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
            region: reachable_blocks_from(func, func.entry())
                .into_iter()
                .collect(),
            parent: None,
            lambda: None,
            params: func.params.clone(),
        }];
        let mut block_scope: HashMap<Idx<Block>, ScopeId> = HashMap::default();
        for &b in &scopes[ROOT].region {
            block_scope.insert(b, ROOT);
        }
        let mut lambda_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
        for (bid, b) in func.blocks.iter() {
            for &iid in &b.insts {
                if matches!(func.insts[iid], Inst::Lambda { .. }) {
                    lambda_block.insert(iid, bid);
                }
            }
        }
        let mut pending: Vec<_> = lambda_block.into_iter().collect();
        pending.sort_unstable_by_key(|&(id, _)| crate::visit::raw(id));
        // A nested lambda becomes live only after its creator's scope is live.
        loop {
            let before = pending.len();
            pending.retain(|&(lid, block)| {
                let Some(&parent) = block_scope.get(&block) else {
                    return true;
                };
                let Inst::Lambda { params, entry } = &func.insts[lid] else {
                    unreachable!()
                };
                let sid = scopes.len();
                let region: HashSet<_> = reachable_blocks_from(func, *entry).into_iter().collect();
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
                    parent: Some(parent),
                    lambda: Some(lid),
                    params: params.clone(),
                });
                false
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        ScopeTree {
            scopes,
            block_scope,
        }
    }

    pub(crate) fn scope_of_block(&self, b: Idx<Block>) -> Option<ScopeId> {
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

    /// Count a map cursor as multiple writes because its instruction repeats.
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
                |c: Idx<Cell>,
                 n: u32,
                 b: Idx<Block>,
                 pos: usize,
                 acc: &mut HashMap<Idx<Cell>, HashSet<ScopeId>>| {
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
                    let inst = &func.insts[iid];
                    if let Inst::Load(cell) = inst {
                        acc.entry(*cell).or_default().insert(sid);
                    } else {
                        let n = 1 + u32::from(matches!(inst, Inst::MapNext { .. }));
                        inst.cell_writes_each(&mut |c| write(c, n, b, pos, &mut acc));
                    }
                }
            }
        }
        // The default fill initializes the parameter before closures can capture it.
        for (cell, data) in func.cells.iter() {
            if data.default_param {
                if let Some(count) = writes.get_mut(&cell) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        (acc, writes, writing, sites)
    }

    pub fn analyze(&self, func: &MirFunc) -> Ownership {
        let (acc, writes, writing_scopes, write_sites) = self.accesses(func);
        let mut owner: HashMap<Idx<Cell>, ScopeId> = HashMap::default();
        for (c, scopes) in &acc {
            let mut it = scopes.iter().copied();
            let first = it.next().expect("a cell has >= 1 accessor");
            let o = it.fold(first, |a, b| self.lca(a, b));
            owner.insert(*c, o);
        }
        // An intervening scope must forward a capture even when it does not read it.
        let mut captures: Vec<HashSet<Idx<Cell>>> = vec![HashSet::default(); self.scopes.len()];
        for (c, scopes) in &acc {
            let o = owner[c];
            for &a in scopes {
                let mut s = a;
                while s != o {
                    captures[s].insert(*c);
                    s = self.scopes[s]
                        .parent
                        .expect("owner is an ancestor of every accessor");
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

    /// Box captures with shared mutation. A read-only loop capture snapshots its iteration.
    pub fn boxed_cells(&self, own: &Ownership) -> HashSet<Idx<Cell>> {
        let mut captured: HashSet<Idx<Cell>> = HashSet::default();
        for caps in &own.captures {
            captured.extend(caps.iter().copied());
        }
        let is_param: HashSet<Idx<Cell>> = self
            .scopes
            .iter()
            .flat_map(|s| s.params.iter().copied())
            .collect();

        captured
            .into_iter()
            .filter(|c| {
                let effective =
                    own.writes.get(c).copied().unwrap_or(0) + is_param.contains(c) as u32;
                let written_non_owner = own
                    .writing_scopes
                    .get(c)
                    .is_some_and(|ss| ss.iter().any(|&s| s != own.owner[c]));
                // A closure that writes a loop binding must steer the live counter.
                if written_non_owner {
                    return true;
                }
                if own.loop_bindings.contains(c) {
                    return false;
                }
                effective >= 2
            })
            .collect()
    }
}

/// Lowering tags loop bindings that snapshot per iteration.
fn loop_binding_cells(func: &MirFunc) -> HashSet<Idx<Cell>> {
    func.cells
        .iter()
        .filter(|(_, cell)| cell.loop_binding)
        .map(|(id, _)| id)
        .collect()
}

/// A capture can snapshot only after its initialization dominates the creator.
/// Transitive captures already have a slot in the parent's environment.
pub(crate) fn non_dominating_by_value_captures(
    func: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    boxed: &HashSet<Idx<Cell>>,
    frame_slots: Option<&HashMap<Idx<Cell>, u32>>,
) -> HashSet<Idx<Cell>> {
    let mut extra: HashSet<Idx<Cell>> = HashSet::default();
    let mut create_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for (bid, b) in func.blocks.iter() {
        for &iid in &b.insts {
            if matches!(func.insts[iid], Inst::Lambda { .. }) {
                create_block.insert(iid, bid);
            }
        }
    }
    // Build dominators only for cross-block writes that can capture by value.
    let mut idoms: HashMap<Idx<Block>, skull_mir::cfg::Dominators> = HashMap::default();
    for (scope_id, s) in tree.scopes.iter().enumerate() {
        let Some(lid) = s.lambda else { continue };
        let parent = s.parent.expect("a lambda scope has a parent");
        let Some(&cblock) = create_block.get(&lid) else {
            continue;
        };
        let sid = tree.scope_of_block(cblock); // sanity: create block is in the parent region
        debug_assert_eq!(
            sid,
            Some(parent),
            "a lambda's creation block lies in its parent scope"
        );
        let parent_entry = tree.scopes[parent].entry;
        for &c in &own.captures[scope_id] {
            if boxed.contains(&c) || own.loop_bindings.contains(&c) || extra.contains(&c) {
                continue;
            }
            // Frame cells are table-mediated (via `s` or a loop-binding
            // snapshot of a frame field) -- never box candidates.
            if frame_slots.is_some_and(|f| f.contains_key(&c)) {
                continue;
            }
            if func.cells[c].default_param {
                continue; // the incoming parameter value dominates the default fill
            }
            if own.owner[&c] != parent {
                continue; // transitive -> arrives via the parent env, always available
            }
            let Some((wblock, wpos)) = own.single_write_site(c) else {
                continue; // multi-write (already handled by boxed_cells) — skip
            };
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
                    .or_insert_with(|| skull_mir::cfg::dominators(func, parent_entry));
                skull_mir::cfg::dominates(idom, wblock, cblock)
            };
            if !write_dominates {
                extra.insert(c);
                debug_assert!(
                    false,
                    "by-value capture write must dominate its creation site (dominance rail)"
                );
                log::warn!(
                    "dominance rail: by-value capture cell {c:?} write does not dominate its creation site; boxing as fallback"
                );
            }
        }
    }
    extra
}
