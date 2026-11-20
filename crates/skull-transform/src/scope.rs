//! Find the scope that owns each cell and the lambdas that capture it.
//! Lambda entries are reached through their creators, outside the enclosing CFG.

use crate::visit::raw;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_mir::mir::*;

use skull_mir::cfg::successors;

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
    creation_block: Option<Idx<Block>>,
}

pub struct ScopeTree {
    pub scopes: Vec<Scope>,
    block_scope: Vec<Option<ScopeId>>,
}

#[derive(Default)]
struct CellUse {
    owner: Option<ScopeId>,
    writer: Option<ScopeId>,
    write_site: Option<(Idx<Block>, usize)>,
    writes: u32,
    is_param: bool,
    default_param: bool,
    loop_binding: bool,
}

impl CellUse {
    fn write(&mut self, scope: ScopeId, block: Idx<Block>, pos: usize, count: u32) {
        if self.writes == 0 {
            self.writer = Some(scope);
            self.write_site = Some((block, pos));
        } else {
            if self.writer != Some(scope) {
                self.writer = None;
            }
            self.write_site = None;
        }
        self.writes += count;
    }

    fn written_outside(&self, scope: ScopeId) -> bool {
        self.writes != 0 && self.writer != Some(scope)
    }
}

/// Ownership is the common ancestor of every scope that uses the cell.
pub struct Ownership {
    cells: Vec<CellUse>,
    pub captures: Vec<HashSet<Idx<Cell>>>,
}

impl Ownership {
    pub fn owner(&self, cell: Idx<Cell>) -> Option<ScopeId> {
        self.cells.get(raw(cell)).and_then(|row| row.owner)
    }

    pub(crate) fn written_outside(&self, cell: Idx<Cell>, scope: ScopeId) -> bool {
        self.cells[raw(cell)].written_outside(scope)
    }

    pub(crate) fn is_loop_binding(&self, cell: Idx<Cell>) -> bool {
        self.cells
            .get(raw(cell))
            .is_some_and(|row| row.loop_binding)
    }
}

impl ScopeTree {
    pub fn build(func: &MirFunc) -> ScopeTree {
        let mut scopes = vec![Scope {
            entry: func.entry(),
            region: HashSet::default(),
            parent: None,
            lambda: None,
            params: func.params.clone(),
            creation_block: None,
        }];
        let mut block_scope = vec![None; func.blocks.len()];
        let mut pending = Vec::new();
        let mut sid = ROOT;
        while sid < scopes.len() {
            pending.push(scopes[sid].entry);
            while let Some(block) = pending.pop() {
                let owner = &mut block_scope[raw(block)];
                if let Some(previous) = *owner {
                    debug_assert_eq!(previous, sid, "lambda CFGs must be disjoint");
                    continue;
                }
                *owner = Some(sid);
                scopes[sid].region.insert(block);
                let block_data = &func.blocks[block];
                for &id in &block_data.insts {
                    if let Inst::Lambda { params, entry } = &func.insts[id] {
                        scopes.push(Scope {
                            entry: *entry,
                            region: HashSet::default(),
                            parent: Some(sid),
                            lambda: Some(id),
                            params: params.clone(),
                            creation_block: Some(block),
                        });
                    }
                }
                successors(&block_data.term, |successor| pending.push(successor));
            }
            sid += 1;
        }
        ScopeTree {
            scopes,
            block_scope,
        }
    }

    pub(crate) fn scope_of_block(&self, b: Idx<Block>) -> Option<ScopeId> {
        self.block_scope.get(raw(b)).copied().flatten()
    }

    fn lca(&self, mut a: ScopeId, mut b: ScopeId) -> ScopeId {
        // Parents are assigned before children, so the larger ID cannot be the ancestor.
        while a != b {
            let later = if a > b { &mut a } else { &mut b };
            *later = self.scopes[*later]
                .parent
                .expect("later scope has a parent");
        }
        a
    }

    pub fn analyze(&self, func: &MirFunc) -> Ownership {
        let mut cells: Vec<_> = func
            .cells
            .values()
            .map(|cell| CellUse {
                default_param: cell.default_param,
                loop_binding: cell.loop_binding,
                ..CellUse::default()
            })
            .collect();
        let mut captures = vec![HashSet::default(); self.scopes.len()];
        for (sid, scope) in self.scopes.iter().enumerate() {
            for &cell in &scope.params {
                cells[raw(cell)].is_param = true;
            }
            let mut access = |cell: Idx<Cell>, write: Option<(Idx<Block>, usize, u32)>| {
                let row = &mut cells[raw(cell)];
                if sid == ROOT {
                    row.owner = Some(ROOT);
                } else if captures[sid].insert(cell) {
                    row.owner = Some(row.owner.map_or(sid, |owner| self.lca(owner, sid)));
                }
                if let Some((block, pos, count)) = write {
                    row.write(sid, block, pos, count);
                }
            };
            for &cell in &scope.params {
                access(cell, None);
            }
            for &block in &scope.region {
                for (pos, &id) in func.blocks[block].insts.iter().enumerate() {
                    let inst = &func.insts[id];
                    if let Inst::Load(cell) = inst {
                        access(*cell, None);
                    } else {
                        // A map cursor is assigned each time its loop runs.
                        let count = 1 + u32::from(matches!(inst, Inst::MapNext { .. }));
                        inst.cell_writes_each(&mut |cell| access(cell, Some((block, pos, count))));
                    }
                }
            }
        }
        // A scope forwards descendant captures up to the owning scope.
        for sid in (1..self.scopes.len()).rev() {
            let (parents, children) = captures.split_at_mut(sid);
            let captured = &mut children[0];
            captured.retain(|&cell| cells[raw(cell)].owner != Some(sid));
            let parent = self.scopes[sid]
                .parent
                .expect("non-root scope has a parent");
            if parent != ROOT {
                parents[parent].extend(captured.iter().copied());
            }
        }
        Ownership { cells, captures }
    }

    /// Box captures with shared mutation. A read-only loop capture snapshots its iteration.
    pub fn boxed_cells(&self, own: &Ownership) -> HashSet<Idx<Cell>> {
        own.captures
            .iter()
            .flatten()
            .copied()
            .filter(|&cell| {
                let row = &own.cells[raw(cell)];
                let owner = row.owner.expect("a captured cell has an owner");
                if row.written_outside(owner) {
                    return true;
                }
                // The default fill initializes a parameter before it can be captured.
                let writes = row.writes.saturating_sub(u32::from(row.default_param));
                !row.loop_binding && writes + u32::from(row.is_param) >= 2
            })
            .collect()
    }
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
    // Build dominators only for cross-block writes that can capture by value.
    let mut idoms: HashMap<Idx<Block>, skull_mir::cfg::Dominators> = HashMap::default();
    for (scope_id, s) in tree.scopes.iter().enumerate() {
        let Some(lid) = s.lambda else { continue };
        let parent = s.parent.expect("a lambda scope has a parent");
        if !matches!(func.insts[lid], Inst::Lambda { .. }) {
            continue;
        }
        let cblock = s.creation_block.expect("lambda has a creation block");
        let parent_entry = tree.scopes[parent].entry;
        for &c in &own.captures[scope_id] {
            if boxed.contains(&c) || own.is_loop_binding(c) || extra.contains(&c) {
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
            if own.owner(c) != Some(parent) {
                continue; // transitive -> arrives via the parent env, always available
            }
            let Some((wblock, wpos)) = own.cells[raw(c)].write_site else {
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
