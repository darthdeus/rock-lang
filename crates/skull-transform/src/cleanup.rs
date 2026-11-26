use crate::copy_prop::{CopyProp, copy_propagate_analysis, resolve_subst};
use crate::sroa::scalarize_struct_cells_once;
use crate::visit::raw;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;
use std::num::NonZeroU32;

pub(crate) fn dce(func: &mut MirFunc) {
    dce_core(func, None);
}

fn dce_core(func: &mut MirFunc, cp: Option<&CopyProp>) -> bool {
    let ni = func.insts.len();
    let nc = func.cells.len();
    let mut use_count = vec![0u32; ni];
    let mut load_count = vec![0u32; nc];
    // Removing the final load needs only the stores of that cell.
    let mut last_store = vec![None; nc];
    let mut stores = Vec::new();
    {
        let MirFunc { blocks, insts, .. } = &mut *func;
        for (_, b) in blocks.iter_mut() {
            for &i in &b.insts {
                if let Some(cp) = cp {
                    insts[i].remap_uses(&|x| resolve_subst(&cp.subst, x));
                }
                insts[i].uses_each(&mut |op| use_count[raw(op)] += 1);
                match &insts[i] {
                    Inst::Load(c) => load_count[raw(*c)] += 1,
                    Inst::Store { cell, .. } => {
                        let next = NonZeroU32::new(stores.len() as u32 + 1);
                        stores.push((i, std::mem::replace(&mut last_store[raw(*cell)], next)));
                    }
                    _ => {}
                }
            }
            if let Some(cp) = cp {
                b.term.remap_uses(&|x| resolve_subst(&cp.subst, x));
            }
            b.term.uses_each(&mut |v| use_count[raw(v)] += 1);
        }
    }
    // Compact templates summarize these loads instead of cloning their dead blocks.
    for &cell in &func.inline_cleanup_load_cells {
        load_count[raw(cell)] = load_count[raw(cell)].saturating_add(1);
    }
    let mut dead = vec![false; ni];
    let mut work: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        for &i in &b.insts {
            if is_dead_rc(&func.insts[i], i, &use_count, &load_count)
                || cp.is_some_and(|cp| {
                    cp.self_stores[raw(i)] || (cp.nonthrowing[raw(i)] && use_count[raw(i)] == 0)
                })
            {
                dead[raw(i)] = true;
                work.push(i);
            }
        }
    }
    let mut exposes = false;
    let mut removed_store_cells: Vec<usize> = Vec::new();
    let mut swept_projections = false;
    loop {
        let Some(i) = work.pop() else {
            if swept_projections {
                break;
            }
            swept_projections = true;
            drop_repeated_projections(func, &use_count, &mut dead, &mut work);
            continue;
        };
        if let Inst::Store { cell, .. } = &func.insts[i] {
            removed_store_cells.push(raw(*cell));
        }
        func.insts[i].uses_each(&mut |op| {
            let k = raw(op);
            exposes |= matches!(func.insts[op], Inst::MakeStruct { .. });
            use_count[k] = use_count[k].saturating_sub(1);
            if !dead[k]
                && use_count[k] == 0
                && (is_dead_value(&func.insts[op]) || cp.is_some_and(|cp| cp.nonthrowing[k]))
            {
                dead[k] = true;
                work.push(op);
            }
        });
        if let Inst::Load(c) = &func.insts[i] {
            let ck = raw(*c);
            load_count[ck] = load_count[ck].saturating_sub(1);
            if load_count[ck] == 0 {
                let mut store = last_store[ck];
                while let Some(link) = store {
                    let (id, prev) = stores[(link.get() - 1) as usize];
                    if !dead[raw(id)] {
                        dead[raw(id)] = true;
                        work.push(id);
                    }
                    store = prev;
                }
            }
        }
    }

    drop_flagged(func, &dead);
    exposes || removed_store_cells.iter().any(|&c| load_count[c] > 0)
}

/// What a projection reads from: a value, or a local cell, whose separate
/// loads read one value while nothing in between writes the cell.
#[derive(Clone, Copy, PartialEq)]
enum ProjectionBase {
    Value(Idx<Inst>),
    Cell(Idx<Cell>),
}

/// An unused read may go when another read of the same base still raises
/// before any intervening side effect or different error.
fn drop_repeated_projections(
    func: &MirFunc,
    use_count: &[u32],
    dead: &mut [bool],
    work: &mut Vec<Idx<Inst>>,
) {
    let mut unused: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        let mut run: Option<ProjectionBase> = None;
        let mut used = false;
        for &id in &b.insts {
            if dead[raw(id)] {
                continue;
            }
            let inst = &func.insts[id];
            let base = match inst {
                Inst::TupleIndex { base, .. } => Some(*base),
                _ => func.field_projection(id).map(|(base, _)| base),
            };
            let Some(base) = base else {
                if !inst.produces_value() || skull_mir::sinking::may_throw(inst) {
                    end_run(used, &mut unused, dead, work);
                    (run, used) = (None, false);
                }
                continue;
            };
            let base = match func.insts[base] {
                Inst::Load(cell) => ProjectionBase::Cell(cell),
                _ => ProjectionBase::Value(base),
            };
            if run != Some(base) {
                end_run(used, &mut unused, dead, work);
                (run, used) = (Some(base), false);
            }
            if use_count[raw(id)] == 0 && func.synthetic_projections.contains(&id) {
                dead[raw(id)] = true;
                work.push(id);
            } else if use_count[raw(id)] == 0 {
                unused.push(id);
            } else {
                used = true;
            }
        }
        end_run(used, &mut unused, dead, work);
    }
}

fn end_run(used: bool, unused: &mut Vec<Idx<Inst>>, dead: &mut [bool], work: &mut Vec<Idx<Inst>>) {
    let kept = usize::from(!used).min(unused.len());
    for &id in &unused[kept..] {
        dead[raw(id)] = true;
        work.push(id);
    }
    unused.clear();
}

fn drop_flagged(func: &mut MirFunc, dead: &[bool]) {
    for (_, block) in func.blocks.iter_mut() {
        block.insts.retain(|&id| !dead[raw(id)]);
    }
}

/// Checked reads and accesses that can call Lua metamethods must run
/// even when the result is unused.
fn index_read_is_checked(kind: &ContainerKind) -> bool {
    !matches!(
        kind,
        ContainerKind::NativeBufferLeaf
            | ContainerKind::VecColumn
            | ContainerKind::VecStrideSlot
            | ContainerKind::VecNonNullInBounds
            | ContainerKind::NativeBufferInBounds
    )
}

// `Call`/`CallValue` run for effect; a checked `Index` must still execute.
fn is_dead_value(inst: &Inst) -> bool {
    match inst {
        Inst::Index { kind, .. } => !index_read_is_checked(kind),
        _ => inst.produces_value() && !skull_mir::sinking::may_throw(inst),
    }
}

pub fn cleanup(func: &mut MirFunc) -> Result<(), CompilerError> {
    cleanup_in(None, func)
}

pub(crate) fn cleanup_in(ws: Option<&Workspace>, func: &mut MirFunc) -> Result<(), CompilerError> {
    // Code after a raising call lowers into a block nothing reaches; its
    // `return` would keep the values it reads alive.
    if has_unreached_code(func) {
        clear_unreachable_blocks(func);
    }
    // A dead outer constructor can keep the inner cursor escaping. Sweep
    // between SROA rounds so nested cursors can split one level at a time.
    let mut stale = true;
    loop {
        let changed = if !skull_mir::pass_disabled("sroa") && has_scalarizable_store(func) {
            scalarize_struct_cells_once(func)?
        } else {
            false
        };
        let folded = fold_known_compares(func);
        if changed || folded || stale {
            let cp = copy_propagate_analysis(func, ws);
            stale = dce_core(func, cp.as_ref()) || cp.as_ref().is_some_and(|cp| cp.exposes_more);
        } else if skull_mir::verify::checks_enabled() {
            assert_round_settled(ws, func);
        }
        if !changed && !folded {
            break;
        }
    }
    // Forwarding a default argument can make its callee prologue constant.
    let mut decided = fold_known_compares(func);
    while fold_const_branches(func) {
        if !decided {
            dce(func);
            break;
        }
        let cp = copy_propagate_analysis(func, ws);
        dce_core(func, cp.as_ref());
        decided = fold_known_compares(func);
    }
    if decided {
        // A decided test that folded no branch still leaves its operands.
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
    let mut insts: Vec<Idx<Inst>> = Vec::new();
    for (_, b) in func.blocks.iter() {
        insts.extend(b.insts.iter().copied().filter(|&i| {
            matches!(
                func.insts[i],
                Inst::StoreIndex {
                    kind: ContainerKind::NativeBufferLeaf,
                    ..
                }
            )
        }));
    }
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

/// Under MIR checks, run the copy-prop/DCE round `cleanup_in` skipped and
/// panic if it would have changed the body: a later change to either pass can
/// expose something the skip predicate does not report.
fn assert_round_settled(ws: Option<&Workspace>, func: &MirFunc) {
    let mut probe = func.clone();
    let cp = copy_propagate_analysis(&probe, ws);
    dce_core(&mut probe, cp.as_ref());
    assert!(
        probe == *func,
        "cleanup skipped a copy-prop/DCE round that changes `{}`:\n{func}\nwould become\n{probe}",
        func.name
    );
}

/// Fold comparisons with known tags or nilness. A value-producing
/// conversion alone does not prove its operand non-null.
pub(crate) fn fold_known_compares(func: &mut MirFunc) -> bool {
    if skull_mir::pass_disabled("foldcmp") {
        return false;
    }
    let never_nil = |func: &MirFunc, id: Idx<Inst>| {
        matches!(func.insts[id], Inst::Lambda { .. })
            || skull_mir::nilness::constructed_never_nil(func, id)
    };
    let is_nil = |func: &MirFunc, v: Idx<Inst>| matches!(&func.insts[v], Inst::Const(Lit::Nil));
    let literal_equality =
        |func: &MirFunc, a: Idx<Inst>, b: Idx<Inst>| match (&func.insts[a], &func.insts[b]) {
            (Inst::Const(lit), Inst::Const(s @ (Lit::Nil | Lit::Omitted))) => Some(lit == s),
            (Inst::Const(Lit::Str(a)), Inst::Const(Lit::Str(b))) => Some(a == b),
            _ => None,
        };

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
        } else if let Some(equal) =
            literal_equality(func, a, b).or_else(|| literal_equality(func, b, a))
        {
            rewrites.push((iid, equal == eq));
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

pub(crate) fn fold_const_branches(func: &mut MirFunc) -> bool {
    if skull_mir::pass_disabled("foldbr") {
        return false;
    }
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
    clear_unreachable_blocks(func);
    true
}

fn has_unreached_code(func: &MirFunc) -> bool {
    let mut reach = vec![false; func.blocks.len()];
    let mut stack = vec![func.entry];
    stack.extend(func.coro_defers.iter().map(|(_, e)| *e));
    for b in &stack {
        reach[raw(*b)] = true;
    }
    while let Some(b) = stack.pop() {
        skull_mir::cfg::successors(&func.blocks[b].term, |t| {
            if !std::mem::replace(&mut reach[raw(t)], true) {
                stack.push(t);
            }
        });
    }
    let holds_code = func.blocks.iter().any(|(b, block)| {
        !reach[raw(b)] && (!block.insts.is_empty() || !matches!(block.term, Term::Unreachable))
    });
    holds_code
        || func.emit_ids.as_ref().is_some_and(|ids| {
            let reached: rustc_hash::FxHashSet<u32> = func
                .blocks
                .iter()
                .filter(|(b, _)| reach[raw(*b)])
                .map(|(b, _)| func.emit_block_id(b))
                .collect();
            ids.forced_cell_blocks.iter().any(|b| !reached.contains(b))
        })
}

fn clear_unreachable_blocks(func: &mut MirFunc) -> bool {
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
    let mut cleared = false;
    for (b, block) in func.blocks.iter_mut() {
        if !reach.contains(&b) {
            cleared |= !block.insts.is_empty() || !matches!(block.term, Term::Unreachable);
            block.insts.clear();
            block.term = Term::Unreachable;
        }
    }
    cleared
}

fn is_dead_rc(inst: &Inst, i: Idx<Inst>, use_count: &[u32], load_count: &[u32]) -> bool {
    match inst {
        Inst::Store { cell, .. } => load_count[raw(*cell)] == 0,
        _ => is_dead_value(inst) && use_count[raw(i)] == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_integer_checks_fold_only_proven_constants() {
        for (literal, unsigned, valid) in [
            (Lit::Int(i32::MIN as i64), false, true),
            (Lit::Int(i32::MAX as i64), false, true),
            (Lit::Int(i32::MAX as i64 + 1), false, false),
            (Lit::Uint(u32::MAX), true, true),
            (Lit::Int(-1), true, false),
            (Lit::Float(1.5), false, false),
            (Lit::Float(f64::NAN), false, false),
            (Lit::Float(f64::INFINITY), true, false),
        ] {
            let mut f = MirFunc::new(ustr::Ustr::from("integer_check"));
            let value = f.emit(f.entry, Inst::Const(literal));
            let callee = f.emit(f.entry, Inst::Global(ustr::Ustr::from(VALUE_INTEGER_CHECK)));
            let flag = f.emit(f.entry, Inst::Const(Lit::Bool(unsigned)));
            let check = f.emit(
                f.entry,
                Inst::CallValue {
                    callee,
                    args: vec![value, flag],
                },
            );
            f.set_term(f.entry, Term::Return(vec![check]));
            cleanup(&mut f).unwrap();
            assert_eq!(!f.blocks[f.entry].insts.contains(&check), valid, "{f}");
            if valid {
                assert_eq!(f.blocks[f.entry].term, Term::Return(vec![value]));
            }
        }
    }

    /// The shape this fold exists for still folds: a test against a value
    /// the body just constructed.
    #[test]
    fn a_presence_test_on_a_fresh_tuple_folds() {
        let mut f = MirFunc::new(ustr::Ustr::from("fresh_tuple"));
        let one = f.emit(f.entry, Inst::Const(Lit::Int(1)));
        let made = f.emit(f.entry, Inst::MakeTuple(vec![one]));
        let nil = f.emit(f.entry, Inst::Const(Lit::Nil));
        let test = f.emit(f.entry, Inst::Eq(made, nil));
        f.set_term(f.entry, Term::Return(vec![test]));

        assert!(fold_known_compares(&mut f), "{f}");
        assert!(
            matches!(f.insts[test], Inst::Const(Lit::Bool(false))),
            "{f}"
        );
    }

    /// `unused = p.a; <between>; return p.b` after cleanup: whether the
    /// unused read survived.
    fn unused_read_survives(between: impl FnOnce(&mut MirFunc, Idx<Inst>)) -> bool {
        let mut f = MirFunc::new(ustr::Ustr::from("covered"));
        let cell = f.new_cell(None);
        let p = f.emit(f.entry, Inst::Load(cell));
        let field = |name: &str| Inst::Field {
            nonthrowing: false,
            base: p,
            field: ustr::Ustr::from(name),
        };
        let unused = f.emit(f.entry, field("a"));
        between(&mut f, p);
        let read = f.emit(f.entry, field("b"));
        f.set_term(f.entry, Term::Return(vec![read]));
        cleanup(&mut f).unwrap();
        f.blocks[f.entry].insts.contains(&unused)
    }

    #[test]
    fn an_unused_read_goes_when_a_later_read_of_its_base_raises_first() {
        assert!(!unused_read_survives(|_, _| {}));
        assert!(!unused_read_survives(|f, p| {
            f.emit(f.entry, Inst::MakeTuple(vec![p]));
        }));
    }

    #[test]
    fn a_run_of_unused_reads_keeps_its_first() {
        let mut f = MirFunc::new(ustr::Ustr::from("all_unused"));
        let cell = f.new_cell(None);
        let p = f.emit(f.entry, Inst::Load(cell));
        let reads: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                f.emit(
                    f.entry,
                    Inst::Field {
                        nonthrowing: false,
                        base: p,
                        field: ustr::Ustr::from(name),
                    },
                )
            })
            .collect();
        f.set_term(f.entry, Term::Return(Vec::new()));
        cleanup(&mut f).unwrap();
        let kept: Vec<bool> = reads
            .iter()
            .map(|r| f.blocks[f.entry].insts.contains(r))
            .collect();
        assert_eq!(kept, [true, false, false], "{f}");
    }
}
