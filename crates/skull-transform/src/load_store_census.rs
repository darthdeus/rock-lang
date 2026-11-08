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
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;
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
        live[raw(inst) as usize] = true;
    }
    for &(inst, cell, is_load) in &before {
        let row = &mut census.rows[func.cells[cell].producer as usize];
        // An access rewritten in place to another cell or another inst is gone.
        let kept =
            live[raw(inst) as usize] && cell_access(&func.insts[inst]) == Some((cell, is_load));
        let spliced = raw(inst) as usize >= spliced_floor;
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
        if (raw(inst) as usize) < inst_floor {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ustr::Ustr;

    #[test]
    fn unlinked_accesses_are_charged_to_their_cell_producer() {
        let mut func = MirFunc::new(Ustr::from("census"));
        let entry = func.entry;
        let leaf = func.new_cell(Some(Ustr::from("p__0")));
        func.cells[leaf].producer = CellProducer::AggregateLeaf;
        let kept = func.new_cell(Some(Ustr::from("x")));
        func.cells[kept].producer = CellProducer::Binding;
        let one = func.emit(entry, Inst::Const(Lit::Int(1)));
        func.emit(
            entry,
            Inst::Store {
                cell: leaf,
                value: one,
            },
        );
        let read = func.emit(entry, Inst::Load(leaf));
        func.emit(
            entry,
            Inst::Store {
                cell: kept,
                value: read,
            },
        );
        let floor = func.insts.len();
        // A spliced forwarding pair: dead once the read is forwarded.
        let forwarded = func.new_cell(Some(Ustr::from("n")));
        func.cells[forwarded].producer = CellProducer::InlineParam;
        func.emit(
            entry,
            Inst::Store {
                cell: forwarded,
                value: one,
            },
        );
        let arg = func.emit(entry, Inst::Load(forwarded));
        let sum = func.emit(entry, Inst::IntAdd(arg, arg));
        func.set_term(entry, Term::Return(vec![sum]));

        let mut census = LoadStoreCensus::default();
        observe_cleanup(&mut func, floor, &mut census, crate::cleanup).expect("cleanup");

        let row = |p: CellProducer| census.rows[p as usize];
        let leaf_row = row(CellProducer::AggregateLeaf);
        assert_eq!((leaf_row.loads_before, leaf_row.stores_before), (1, 1));
        assert_eq!((leaf_row.loads_unlinked, leaf_row.stores_unlinked), (1, 1));
        assert_eq!(
            leaf_row.spliced_loads_unlinked + leaf_row.spliced_stores_unlinked,
            0
        );
        let binding = row(CellProducer::Binding);
        assert_eq!((binding.stores_before, binding.stores_unlinked), (1, 1));
        let param = row(CellProducer::InlineParam);
        assert_eq!((param.loads_unlinked, param.stores_unlinked), (1, 1));
        assert_eq!(
            (param.spliced_loads_unlinked, param.spliced_stores_unlinked),
            (1, 1)
        );
        assert!(row(CellProducer::Temp).is_empty());
    }

    #[test]
    fn observed_cleanup_matches_unobserved_cleanup() {
        let build = || {
            let mut func = MirFunc::new(Ustr::from("same"));
            let entry = func.entry;
            let cell = func.new_cell(Some(Ustr::from("x")));
            let one = func.emit(entry, Inst::Const(Lit::Int(1)));
            func.emit(entry, Inst::Store { cell, value: one });
            let read = func.emit(entry, Inst::Load(cell));
            func.set_term(entry, Term::Return(vec![read]));
            func
        };
        let mut plain = build();
        crate::cleanup(&mut plain).expect("cleanup");
        let mut observed = build();
        observe_cleanup(
            &mut observed,
            0,
            &mut LoadStoreCensus::default(),
            crate::cleanup,
        )
        .expect("cleanup");
        assert_eq!(plain, observed);
    }
}
