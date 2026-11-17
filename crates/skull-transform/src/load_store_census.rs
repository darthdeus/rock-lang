//! Producer attribution for the `Load`/`Store` instructions the pre-emit cleanup
//! unlinks. Measurement only: it observes a cleanup run and never changes what
//! the cleanup does. A cell's [`CellProducer`] names the lowering or inlining
//! code that minted it; the census charges each unlinked access to its cell's
//! producer, and separately counts accesses the inline splice created.
//!
//! Unlinked counts say where the cleanup spends work, not that a producer could
//! skip the cell: mutation, aliasing and snapshot semantics still decide that.

use crate::nobox::cleanup_with_nobox;
use crate::visit::raw;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStoreRow {
    /// Live accesses to this producer's cells when cleanup started.
    pub loads_before: usize,
    pub stores_before: usize,
    /// Of those, accesses cleanup removed or rewrote away.
    pub loads_unlinked: usize,
    pub stores_unlinked: usize,
    /// Of the unlinked, accesses an inline splice created (cloned template
    /// code, argument forwarding stores, result reads).
    pub spliced_loads_unlinked: usize,
    pub spliced_stores_unlinked: usize,
    /// Accesses cleanup itself introduced (SROA field traffic).
    pub loads_added: usize,
    pub stores_added: usize,
}

impl LoadStoreRow {
    fn add(&mut self, other: Self) {
        self.loads_before += other.loads_before;
        self.stores_before += other.stores_before;
        self.loads_unlinked += other.loads_unlinked;
        self.stores_unlinked += other.stores_unlinked;
        self.spliced_loads_unlinked += other.spliced_loads_unlinked;
        self.spliced_stores_unlinked += other.spliced_stores_unlinked;
        self.loads_added += other.loads_added;
        self.stores_added += other.stores_added;
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One row per [`CellProducer`], indexed by `producer as usize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadStoreCensus {
    pub rows: [LoadStoreRow; CellProducer::ALL.len()],
}

impl LoadStoreCensus {
    pub fn add(&mut self, other: &Self) {
        for (row, other) in self.rows.iter_mut().zip(other.rows) {
            row.add(other);
        }
    }
}

fn cell_access(inst: &Inst) -> Option<(Idx<Cell>, bool)> {
    match inst {
        Inst::Load(cell) => Some((*cell, true)),
        Inst::Store { cell, .. } => Some((*cell, false)),
        _ => None,
    }
}

/// Live `Load`/`Store` instructions with their cell and kind.
fn live_accesses(func: &MirFunc) -> Vec<(Idx<Inst>, Idx<Cell>, bool)> {
    let mut out = Vec::new();
    for (_, block) in func.blocks.iter() {
        for &inst in &block.insts {
            if let Some((cell, is_load)) = cell_access(&func.insts[inst]) {
                out.push((inst, cell, is_load));
            }
        }
    }
    out
}

/// Run `cleanup` on `func`, charging the accesses it unlinks to `census`.
/// Instructions at or past `spliced_floor` were created by inlining.
pub fn observe_cleanup(
    func: &mut MirFunc,
    spliced_floor: usize,
    census: &mut LoadStoreCensus,
    cleanup: impl FnOnce(&mut MirFunc) -> Result<(), CompilerError>,
) -> Result<(), CompilerError> {
    let before = live_accesses(func);
    let inst_floor = func.insts.len();
    cleanup(func)?;
    let mut live = vec![false; func.insts.len()];
    for (inst, _, _) in live_accesses(func) {
        live[raw(inst)] = true;
    }
    for &(inst, cell, is_load) in &before {
        let row = &mut census.rows[func.cells[cell].producer as usize];
        // An access rewritten in place to another cell or another inst is gone.
        let kept = live[raw(inst)] && cell_access(&func.insts[inst]) == Some((cell, is_load));
        let spliced = raw(inst) >= spliced_floor;
        match (is_load, kept) {
            (true, _) => row.loads_before += 1,
            (false, _) => row.stores_before += 1,
        }
        if kept {
            continue;
        }
        if is_load {
            row.loads_unlinked += 1;
            row.spliced_loads_unlinked += usize::from(spliced);
        } else {
            row.stores_unlinked += 1;
            row.spliced_stores_unlinked += usize::from(spliced);
        }
    }
    for (inst, cell, is_load) in live_accesses(func) {
        if (raw(inst)) < inst_floor {
            continue;
        }
        let row = &mut census.rows[func.cells[cell].producer as usize];
        if is_load {
            row.loads_added += 1;
        } else {
            row.stores_added += 1;
        }
    }
    Ok(())
}

/// [`cleanup_with_nobox`], observed by `census` when one is given.
pub fn cleanup_with_nobox_census(
    ws: &Workspace,
    func: &mut MirFunc,
    spliced_floor: usize,
    census: Option<&mut LoadStoreCensus>,
) -> Result<(), CompilerError> {
    match census {
        Some(census) => observe_cleanup(func, spliced_floor, census, |func| {
            cleanup_with_nobox(ws, func)
        }),
        None => cleanup_with_nobox(ws, func),
    }
}
