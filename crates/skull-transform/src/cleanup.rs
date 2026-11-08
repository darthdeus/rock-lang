use crate::copy_prop::{CopyProp, copy_propagate_analysis, resolve_subst};
use crate::native_buffer::scalarize_native_buffer_stores;
use crate::sroa::scalarize_struct_cells_once;
use crate::visit::{raw, remap_inst, remap_term};
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::mir::*;

/// Dead-code elimination over a [`MirFunc`] (a CFG->CFG cleanup, run before emit).
/// Removes, to a fixpoint: (a) pure value-producing insts with no users — the
/// dead `Load`/`Const`/arith/`Field` left by the cell-model's literal lowering;
/// (b) `Store` to a cell that is never `Load`ed anywhere — the dead result cells
/// of statement-position control flow. Side-effecting insts (`Call`/`CallValue`/
/// `CallMulti`/`StoreField`/`StoreIndex`/`StoreGlobal`/`MapNext`/`ArrayPush`/
/// coroutine ops) are always kept (an unused call result still runs the call).
/// This shrinks LuaJIT's snapshot width (fewer live locals at a side-exit), the
/// named trigger of skull's trace-cache thrash. Scans ALL blocks (incl. closure
/// sub-CFGs in the same arena), so a value/cell used only inside a lambda body
/// is correctly seen as live.
pub fn dce(func: &mut MirFunc) {
    dce_core(func, None);
}

/// DCE, optionally fusing a copy-prop analysis into the count scan: with [`CopyProp`]
/// the value operands (and each terminator's) are remapped through its
/// substitution as they are counted, and its proven no-op stores are seeded
/// dead -- so DCE's post-substitution reference counts and the store deletion
/// fall out of work DCE needs anyway. With `None` it is plain DCE.
fn dce_core(func: &mut MirFunc, cp: Option<&CopyProp>) {
    let ni = func.insts.len();
    let nc = func.cells.len();
    // Reference counts over the CURRENT program (dense, raw-id keyed): live value
    // uses per inst, `Load(c)` count per cell, and stores per cell (for the CSR
    // index below). A value/store dies when its count reaches 0; removing an inst
    // decrements the counts of what it referenced, cascading via a worklist — so
    // the load->store->value chains the cell model leaves converge in ONE pass
    // instead of the old fixpoint's ~2.2 full re-scans.
    let mut use_count = vec![0u32; ni];
    let mut load_count = vec![0u32; nc];
    let mut store_n = vec![0u32; nc];
    {
        // Split the `&mut MirFunc` into independent field borrows so a `Store`
        // value can be remapped in `insts` while iterating `blocks` (a term is
        // remapped through `&mut b.term`). Cells/opcodes are never rewritten, so
        // the Load/Store accounting below is unaffected by the substitution.
        let MirFunc { blocks, insts, .. } = &mut *func;
        for (_, b) in blocks.iter_mut() {
            for &i in &b.insts {
                if let Some(cp) = cp {
                    remap_inst(&mut insts[i], &|x| resolve_subst(&cp.subst, x));
                }
                insts[i].uses_each(&mut |op| use_count[raw(op) as usize] += 1);
                match &insts[i] {
                    Inst::Load(c) => load_count[raw(*c) as usize] += 1,
                    Inst::Store { cell, .. } => store_n[raw(*cell) as usize] += 1,
                    _ => {}
                }
            }
            if let Some(cp) = cp {
                remap_term(&mut b.term, &|x| resolve_subst(&cp.subst, x));
            }
            // Terminator operands are permanent uses (terms are never removed), so
            // their +1 is never decremented — a value used by a term stays live.
            b.term.uses_each(&mut |v| use_count[raw(v) as usize] += 1);
        }
    }
    // Compact templates summarize these loads instead of cloning their dead blocks.
    for &cell in &func.inline_cleanup_load_cells {
        load_count[raw(cell) as usize] = load_count[raw(cell) as usize].saturating_add(1);
    }
    // CSR: stores grouped by cell, so a cell whose last `Load` is removed cascades
    // its now-dead stores in O(stores-of-that-cell).
    let mut store_off = vec![0u32; nc + 1];
    for c in 0..nc {
        store_off[c + 1] = store_off[c] + store_n[c];
    }
    let mut store_list = vec![0u32; store_off[nc] as usize];
    let mut fill = store_off.clone();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if let Inst::Store { cell, .. } = &func.insts[i] {
                let c = raw(*cell) as usize;
                store_list[fill[c] as usize] = raw(i);
                fill[c] += 1;
            }
        }
    }

    // Seed the worklist with everything dead in the initial program, then cascade;
    // a store proven a no-op by the copy-prop analysis is dead whatever its cell's
    // load count.
    let mut dead = vec![false; ni];
    let mut work: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if is_dead_rc(&func.insts[i], i, &use_count, &load_count)
                || cp.is_some_and(|cp| {
                    cp.self_stores[raw(i) as usize]
                        || (cp.nonthrowing[raw(i) as usize] && use_count[raw(i) as usize] == 0)
                })
            {
                dead[raw(i) as usize] = true;
                work.push(i);
            }
        }
    }
    while let Some(i) = work.pop() {
        // Each value operand of the removed inst loses a use; a value that hits
        // zero (and is pure) is itself now dead.
        func.insts[i].uses_each(&mut |op| {
            let k = raw(op) as usize;
            use_count[k] = use_count[k].saturating_sub(1);
            if !dead[k]
                && use_count[k] == 0
                && (is_dead_value(&func.insts[op]) || cp.is_some_and(|cp| cp.nonthrowing[k]))
            {
                dead[k] = true;
                work.push(op);
            }
        });
        // Removing a `Load` drops its cell's load count; a cell with no remaining
        // loads makes its stores dead (their value operands then cascade on pop).
        if let Inst::Load(c) = &func.insts[i] {
            let ck = raw(*c) as usize;
            load_count[ck] = load_count[ck].saturating_sub(1);
            if load_count[ck] == 0 {
                for si in store_off[ck]..store_off[ck + 1] {
                    let s = Idx::from_raw(RawIdx::from_u32(store_list[si as usize]));
                    if !dead[raw(s) as usize] {
                        dead[raw(s) as usize] = true;
                        work.push(s);
                    }
                }
            }
        }
    }

    drop_flagged(func, &dead);
}

/// Remove every instruction whose `dead` flag is set from its block.
fn drop_flagged(func: &mut MirFunc, dead: &[bool]) {
    let block_ids: Vec<Idx<Block>> = func.blocks.iter().map(|(id, _)| id).collect();
    for bid in block_ids {
        let mut insts = std::mem::take(&mut func.blocks[bid].insts);
        insts.retain(|&i| !dead[raw(i) as usize]);
        func.blocks[bid].insts = insts;
    }
}

/// Whether an `Index` read must execute even when its result is unused -- i.e.
/// whether the bounds check rides ON this inst. Every ordinary container index
/// errors on OOB, so it does.
///
/// LEAF reads are the exception: one ELEMENT access is decomposed into K leaf
/// reads sharing ONE hoisted check (`__nb_checked_index` for a NativeBuffer;
/// `Inst::VecCheckedIndex` for columns and strided rows -- "one call per
/// ELEMENT access, not per column"). Each leaf read is then a raw pointer read
/// carrying no error of its own, so reading one field of a K-leaf element would
/// otherwise emit all K loads and discard K-1. The shared check itself stays
/// live, so an element read whose leaves are all unused still raises.
///
/// Consulted by BOTH [`is_dead_rc`] (initial seeding) and [`is_dead_value`]
/// (cascade). They must agree: a kind dead in one and live in the other either
/// leaks the loads back or deletes a real bounds error.
fn index_read_is_checked(kind: &ContainerKind) -> bool {
    !matches!(
        kind,
        ContainerKind::NativeBufferLeaf | ContainerKind::VecColumn | ContainerKind::VecStrideSlot
    )
}

/// Whether a value-producing inst is pure enough that an unused result is dead.
/// `Call`/`CallValue` run for effect; a checked `Index` must still execute.
fn is_dead_value(inst: &Inst) -> bool {
    match inst {
        Inst::Index { kind, .. } => !index_read_is_checked(kind),
        _ => inst.produces_value() && !skull_mir::sinking::may_throw(inst),
    }
}

/// The pre-emit MIR cleanup: copy-propagate intra-block loads, then DCE the dead
/// loads/values/stores they leave (including the stores copy propagation proves
/// write back the value their cell already holds). Run per-function in `mir_emit`.
///
/// The two passes are FUSED: copy-prop's substitution is applied as DCE's count
/// scan walks each inst, so the value-operand remap (`remap_inst` over every
/// inst) and the use-counting share one traversal instead of two back-to-back.
/// The standalone `dce` keeps its independent behavior.
pub fn cleanup(func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(None, func)
}

pub(crate) fn cleanup_in(ws: Option<&Workspace>, func: &mut MirFunc) -> Result<(), CompilerError> {
    scalarize_native_buffer_stores(func);
    // SROA and copy-prop/DCE INTERLEAVE to a fixpoint: peeling a cursor struct
    // leaves its (now-dead) ctor behind, and that ctor's operand uses pin the
    // NEXT nesting level's cell as escaping (iterator combinator chains nest
    // cursors: MapIter{__inner: VecIter{..}}). Sweeping between rounds lets
    // each level peel; depth = chain nesting, so this converges fast. The ctor
    // store prefilter avoids allocating the per-cell analysis for no-op rounds.
    loop {
        let changed = if has_scalarizable_store(func) {
            scalarize_struct_cells_once(func)?
        } else {
            false
        };
        // Inside the loop, because SROA is what CREATES the shape: forwarding a
        // nullable value field's read to the literal that filled it leaves a
        // presence test whose operand is now a fresh ctor.
        let folded = fold_never_nil_compares(func);
        let cp = copy_propagate_analysis(func, ws);
        dce_core(func, cp.as_ref());
        if !changed && !folded {
            break;
        }
    }
    if fold_const_branches(func) {
        dce(func);
    }
    Ok(())
}

/// A store through a NativeBuffer's `float` leaf view converts to f32 itself,
/// so an `f32` write's explicit rounding feeding it is redundant: an inlined
/// constructor's per-leaf rounding meets the buffer store here.
pub(crate) fn drop_f32_rounding_into_float_views(func: &mut MirFunc) -> bool {
    let float_view = skull_mir::lower::leaf_view_field("float");
    let mut changed = false;
    let insts: Vec<Idx<Inst>> = func
        .blocks
        .iter()
        .flat_map(|(_, b)| b.insts.iter().copied())
        .collect();
    for i in insts {
        let Inst::StoreIndex {
            base,
            value,
            kind: ContainerKind::NativeBufferLeaf,
            ..
        } = func.insts[i]
        else {
            continue;
        };
        if !matches!(func.insts[base], Inst::Field { field, .. } if field == float_view) {
            continue;
        }
        if let Inst::FloatToF32(unrounded) = func.insts[value] {
            if let Inst::StoreIndex { value, .. } = &mut func.insts[i] {
                *value = unrounded;
                changed = true;
            }
        }
    }
    changed
}

fn has_scalarizable_store(func: &MirFunc) -> bool {
    func.blocks.iter().any(|(_, b)| {
        b.insts.iter().any(|&i| {
            matches!(
                &func.insts[i],
                Inst::Store { value, .. }
                    if matches!(
                        &func.insts[*value],
                        Inst::MakeValueStruct { .. }
                            | Inst::MakeStruct { .. }
                            | Inst::MakeEnum { .. }
                            | Inst::MakeEnumDyn { .. }
                            | Inst::MakeTuple(_)
                    )
            )
        })
    })
}

/// Fold a nil test whose operand is a freshly CONSTRUCTED aggregate: a ctor result
/// is never nil, so `Eq(ctor, nil)` is statically false and `NotEq` statically true.
/// [`fold_const_branches`] then turns the branch into a `Goto` and DCE removes the
/// unreachable arm along with the boxes only it used.
///
/// This exists because SROA manufactures the shape rather than the lowering emitting
/// it: a presence-aware read of a nullable value field tests `Inst::Field` against
/// nil, which is a real test, and forwarding that field read to the literal behind
/// it turns the operand into a ctor. 18 generated shapes boxed a value twice and
/// branched on the result purely for this, and the value oracle read every one as
/// CORRECT because the ANSWER was right.
fn fold_never_nil_compares(func: &mut MirFunc) -> bool {
    let never_nil = |func: &MirFunc, v: Idx<Inst>| {
        matches!(
            &func.insts[v],
            Inst::MakeValueStruct { .. }
                | Inst::MakeStruct { .. }
                | Inst::MakeEnum { .. }
                | Inst::MakeTuple(_)
        )
    };
    let is_nil = |func: &MirFunc, v: Idx<Inst>| matches!(&func.insts[v], Inst::Const(Lit::Nil));

    let mut rewrites: Vec<(Idx<Inst>, bool)> = Vec::new();
    for (iid, inst) in func.insts.iter() {
        let (a, b, eq) = match inst {
            Inst::Eq(a, b) => (*a, *b, true),
            Inst::NotEq(a, b) => (*a, *b, false),
            _ => continue,
        };
        let known =
            (never_nil(func, a) && is_nil(func, b)) || (never_nil(func, b) && is_nil(func, a));
        if known {
            rewrites.push((iid, !eq));
        }
    }
    if rewrites.is_empty() {
        return false;
    }
    for (iid, value) in rewrites {
        func.insts[iid] = Inst::Const(Lit::Bool(value));
    }
    true
}

/// Fold `Branch` on a constant bool condition into a `Goto`, then clear blocks
/// no longer reachable from any root (fn entry, lambda entries, co-fn defer
/// guards -- the latter two are detached sub-CFGs by design and must survive).
/// `loop {}` lowers to a `while true` shape whose const branch hides the loop
/// head from the FORL matcher's goto-chase (the filter combinator's skip loop)
/// and leaves a dead exit arm in the emitted Lua. Returns whether anything
/// changed (the dead `Const` conditions are left for a DCE round).
pub fn fold_const_branches(func: &mut MirFunc) -> bool {
    let bids: Vec<Idx<Block>> = func.blocks.iter().map(|(b, _)| b).collect();
    let mut changed = false;
    for b in &bids {
        if let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = func.blocks[*b].term
        {
            if let Inst::Const(Lit::Bool(v)) = func.insts[cond] {
                func.blocks[*b].term = Term::Goto(if v { then_bb } else { else_bb });
                changed = true;
            }
        }
    }
    if !changed {
        return false;
    }
    let mut roots = vec![func.entry];
    roots.extend(func.coro_defers.iter().map(|(_, e)| *e));
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { entry, .. } = inst {
            roots.push(*entry);
        }
    }
    let mut reach: rustc_hash::FxHashSet<Idx<Block>> = roots.iter().copied().collect();
    let mut stack = roots;
    while let Some(b) = stack.pop() {
        skull_mir::cfg::successors(&func.blocks[b].term, |t| {
            if reach.insert(t) {
                stack.push(t);
            }
        });
    }
    let reachable_emit_blocks: rustc_hash::FxHashSet<u32> = reach
        .iter()
        .map(|block| func.emit_block_id(*block))
        .collect();
    if let Some(ids) = &mut func.emit_ids {
        debug_assert_eq!(ids.forced_cells.len(), ids.forced_cell_blocks.len());
        let mut forced_cells = Vec::with_capacity(ids.forced_cells.len());
        let mut forced_cell_blocks = Vec::with_capacity(ids.forced_cell_blocks.len());
        for (&cell, &block) in ids.forced_cells.iter().zip(&ids.forced_cell_blocks) {
            if reachable_emit_blocks.contains(&block) {
                forced_cells.push(cell);
                forced_cell_blocks.push(block);
            }
        }
        ids.forced_cells = forced_cells;
        ids.forced_cell_blocks = forced_cell_blocks;
    }
    for b in bids {
        if !reach.contains(&b) {
            func.blocks[b].insts.clear();
            func.blocks[b].term = Term::Unreachable;
        }
    }
    true
}

/// Whether instruction `i` is removable given the current reference counts: a
/// pure value-producing inst with no users (`use_count == 0`), or a `Store` to a
/// never-loaded cell (`load_count == 0`). `Call`/`CallValue` produce a value but
/// have side effects, so an unused result does NOT make them dead; a bounds-
/// checked `Index` read can error, so a discarded read must still run.
fn is_dead_rc(inst: &Inst, i: Idx<Inst>, use_count: &[u32], load_count: &[u32]) -> bool {
    match inst {
        Inst::Store { cell, .. } => load_count[raw(*cell) as usize] == 0,
        _ => is_dead_value(inst) && use_count[raw(i) as usize] == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower;

    #[test]
    fn cleanup_load_root_keeps_the_written_cell() {
        let mut unrooted = MirFunc::new(ustr::Ustr::from("cleanup_root"));
        let cell = unrooted.new_cell(None);
        let value = unrooted.emit(unrooted.entry, Inst::Const(Lit::Int(1)));
        let store = unrooted.emit(unrooted.entry, Inst::Store { cell, value });
        unrooted.set_term(unrooted.entry, Term::Return(Vec::new()));
        let _ = cleanup(&mut unrooted);
        assert!(!unrooted.blocks[unrooted.entry].insts.contains(&store));

        let mut rooted = MirFunc::new(ustr::Ustr::from("cleanup_root"));
        let cell = rooted.new_cell(None);
        let value = rooted.emit(rooted.entry, Inst::Const(Lit::Int(1)));
        let store = rooted.emit(rooted.entry, Inst::Store { cell, value });
        rooted.inline_cleanup_load_cells.push(cell);
        rooted.set_term(rooted.entry, Term::Return(Vec::new()));
        let _ = cleanup(&mut rooted);
        assert!(rooted.blocks[rooted.entry].insts.contains(&store));
    }

    #[test]
    fn cleanup_prefilter_finds_only_aggregate_stores() {
        let scalar = lower("fn f(x: int) -> int { return x }", "f");
        assert!(!has_scalarizable_store(&scalar));

        let aggregate = lower(
            "struct Pair { a: int, b: int }\n\
             fn f(flag: bool) -> int {\n\
                 let p = Pair { a: 1, b: 2 }\n\
                 if flag { return p.a }\n\
                 return p.b\n\
             }",
            "f",
        );
        assert!(has_scalarizable_store(&aggregate), "{aggregate}");
    }
}
