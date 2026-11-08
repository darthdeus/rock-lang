//! Which instructions render inline at their use, and fold-depth bounds.

use super::*;

pub(crate) fn always_inlinable(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Const(_)
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::IntDiv(..)
            | Inst::IntMod(..)
            | Inst::UintAdd(..)
            | Inst::UintSub(..)
            | Inst::UintMul(..)
            | Inst::UintDiv(..)
            | Inst::UintMod(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::FloatMod(..)
            | Inst::StringConcat(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::UintLt(..)
            | Inst::UintLeq(..)
            | Inst::UintGt(..)
            | Inst::UintGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::BitAnd(..)
            | Inst::BitOr(..)
            | Inst::BitXor(..)
            | Inst::Shl(..)
            | Inst::Shr(..)
            | Inst::UintBitAnd(..)
            | Inst::UintBitOr(..)
            | Inst::UintBitXor(..)
            | Inst::UintShl(..)
            | Inst::UintShr(..)
            | Inst::Neg(_)
            | Inst::Not(_)
            | Inst::BitNot(_)
            | Inst::UintBitNot(_)
            | Inst::ToString(_)
            | Inst::FloatToInt(_)
            | Inst::IntToFloat(_)
            | Inst::UintToFloat(_)
            | Inst::FloatToF32(_)
            | Inst::IntToUint(_)
            | Inst::UintToInt(_)
            | Inst::FloatToUint(_)
            | Inst::UintBitcastInt(_)
            | Inst::IntBitcastUint(_)
            | Inst::Format { .. }
            | Inst::FormatDyn { .. }
            | Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeNullableValue { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

/// Cells written exactly once in `scope`, as a multi-return destination, and
/// read only after that call in the same block. A load of one is the call's
/// result local itself, like a parameter load: without this every leaf of a
/// wide value returned by a call occupied two locals (the result and a copy),
/// which is what pushed functions holding a few wide values past LuaJIT's
/// 250-slot frame limit. No later write can intervene, because the one write
/// dominates every read. Narrow receives qualify too.
fn single_receive_cells(
    func: &MirFunc,
    scope: &[Idx<Block>],
    pinned: &[bool],
) -> HashSet<Idx<Cell>> {
    // cell -> (defs, the receive's block and position). Only a receive records
    // a position, so a cell qualifies iff its one def is one.
    let mut defs: rustc_hash::FxHashMap<Idx<Cell>, (u32, Option<(Idx<Block>, usize)>)> =
        rustc_hash::FxHashMap::default();
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            match &func.insts[id] {
                Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => {
                    for cell in dests {
                        let entry = defs.entry(*cell).or_insert((0, None));
                        entry.0 += 1;
                        entry.1 = Some((b, pos));
                    }
                }
                // A placeholder the bound substitution replaces; never a receive.
                Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        defs.entry(*cell).or_insert((0, None)).0 += 1;
                    }
                }
                other => regalloc::cell_defs(other, &mut |cell| {
                    defs.entry(cell).or_insert((0, None)).0 += 1;
                }),
            }
        }
    }
    let captured = |cell: Idx<Cell>| {
        func.closure_env_plans
            .values()
            .any(|plan| plan.captures_cell(cell) || plan.env_cell() == Some(cell))
    };
    let mut out: HashSet<Idx<Cell>> = defs
        .iter()
        .filter(|(cell, (count, at))| {
            *count == 1 && at.is_some() && !pinned[raw(**cell) as usize] && !captured(**cell)
        })
        .map(|(cell, _)| *cell)
        .collect();
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            regalloc::cell_uses(&func.insts[id], &mut |cell| {
                if !out.contains(&cell) {
                    return;
                }
                let Some((_, Some((def_block, def_pos)))) = defs.get(&cell) else {
                    return;
                };
                if *def_block != b || *def_pos >= pos || !matches!(func.insts[id], Inst::Load(_)) {
                    out.remove(&cell);
                }
            });
        }
    }
    out
}

/// The set of values to TREE-FOLD into their use site (no `local rN`). Expression
/// candidates qualify at one same-block use, subject to the shared order check.
/// Loads of parameters never written in this scope qualify at every use: the
/// Lua parameter is already their value.
/// Returns the folded values, and the folded loads that may be rendered at any
/// number of use sites: a parameter never written here, or a cell whose one
/// write dominates every read.
pub(crate) fn compute_inlinable(
    func: &MirFunc,
    scope: &[Idx<Block>],
    params: &[Idx<Cell>],
    pinned: &[bool],
) -> (Vec<bool>, Vec<bool>) {
    // Dense side-tables keyed by raw inst id (no hashing): use count, and the
    // block of the (single) use and of the def. `NONE` = unset. Insts outside
    // `scope` (other closures share the arena) stay at 0/NONE and never qualify.
    const NONE: u32 = u32::MAX;
    let ni = func.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    let mut def_block = vec![NONE; ni];
    let mut immutable_params: HashSet<Idx<Cell>> = params.iter().copied().collect();
    for &b in scope {
        let br = raw(b);
        for &id in &func.blocks[b].insts {
            match &func.insts[id] {
                Inst::Store { cell, .. } => {
                    immutable_params.remove(cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        immutable_params.remove(cell);
                    }
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    immutable_params.remove(key_cell);
                    immutable_params.remove(val_cell);
                }
                _ => {}
            }
            if func.insts[id].produces_value() {
                def_block[raw(id) as usize] = br;
            }
            regalloc::inst_uses(&func.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = br;
            });
            // Selected-layout emission may read the dynamic tag more than once.
            if let Inst::MakeEnumDyn { tag, .. } = func.insts[id] {
                if !matches!(func.insts[tag], Inst::Const(_)) {
                    use_count[raw(tag) as usize] += 1;
                }
            }
        }
        regalloc::term_uses(&func.blocks[b].term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = br;
        });
    }
    let received = single_receive_cells(func, scope, pinned);
    // Dense `raw(inst) -> bool` (no hashing); probed per operand in `write_opnd`
    // and per use in `color_values`'s liveness/interference loops.
    let mut inl = vec![false; ni];
    let mut repeatable = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            let parameter_load = matches!(
                &func.insts[id],
                Inst::Load(cell) if immutable_params.contains(cell) || received.contains(cell)
            );
            repeatable[k] = parameter_load;
            if parameter_load
                || (use_count[k] == 1
                    && def_block[k] != NONE
                    && def_block[k] == use_block[k]
                    && always_inlinable(&func.insts[id]))
            {
                inl[k] = true;
            }
        }
    }
    // A folded operand renders parenthesized, and a statement opening with `(`
    // is ambiguous Lua after a line that ends in an expression. A store base
    // other than a plain local therefore keeps its own register.
    for &b in scope {
        for &id in &func.blocks[b].insts {
            if let Inst::StoreField { base, .. } | Inst::StoreIndex { base, .. } = func.insts[id]
                && !matches!(func.insts[base], Inst::Load(_))
            {
                inl[raw(base) as usize] = false;
            }
        }
    }
    (inl, repeatable)
}

/// Fold every `IntToFloat` whose operand renders as a bare local (a register
/// or a `repeatable` load) into its uses, however many: the conversion is the
/// identity on LuaJIT numbers, and its own register would only copy that
/// local. Folded reads see through to the operand, so its register stays live
/// to the last use of the conversion. A value live across a yield is already
/// spilled to a frame field by the coroutine transform, so a resumed step
/// reads the field, never the register. An operand folded into the
/// conversion (`(a + b) as float`) keeps the conversion's register, so it is
/// still evaluated once.
pub(crate) fn alias_int_to_float(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inlinable: &mut [bool],
    repeatable: &[bool],
) {
    for &b in scope {
        for &id in &func.blocks[b].insts {
            if let Inst::IntToFloat(x) = func.insts[id] {
                let x = raw(x) as usize;
                if !inlinable[x] || repeatable[x] {
                    inlinable[raw(id) as usize] = true;
                }
            }
        }
    }
}

/// Append the operands of the flat `..` run `id` renders as: a folded child
/// `StringConcat` contributes its own operands, anything else is one operand.
pub(crate) fn concat_run(
    func: &MirFunc,
    inlinable: &[bool],
    parts: &mut Vec<Idx<Inst>>,
    id: Idx<Inst>,
) {
    if inlinable[raw(id) as usize] {
        if let Inst::StringConcat(a, b) = func.insts[id] {
            concat_run(func, inlinable, parts, a);
            concat_run(func, inlinable, parts, b);
            return;
        }
    }
    parts.push(id);
}

/// Each operand of the folded value `id`, with the LuaJIT syntax levels its
/// rendering nests that operand under, not counting the parenthesis
/// `write_opnd` wraps `id` in. A binary operator's left operand is parsed in
/// its own loop and costs nothing; the right operand, a call argument list, a
/// table constructor and a parenthesis cost one each, so the default allows
/// two; a `..` run's operand costs its position. Allocation instrumentation
/// wraps an allocating value in `(<site> and ..)`, counted always so the
/// instrumented and plain builds fold alike.
fn fold_operand_levels(func: &MirFunc, inlinable: &[bool], id: Idx<Inst>) -> Vec<(Idx<Inst>, u32)> {
    let inst = &func.insts[id];
    let instrument = if emitted_value_allocation_kind(inst).is_some() {
        2
    } else {
        0
    };
    let uniform = |levels: u32| {
        let mut out = Vec::new();
        regalloc::inst_uses(inst, &mut |u| out.push((u, levels)));
        out
    };
    match *inst {
        Inst::StringConcat(a, b) => {
            let mut run = Vec::new();
            concat_run(func, inlinable, &mut run, a);
            concat_run(func, inlinable, &mut run, b);
            run.into_iter().zip(instrument..).collect()
        }
        Inst::IntAdd(a, b)
        | Inst::FloatAdd(a, b)
        | Inst::IntSub(a, b)
        | Inst::FloatSub(a, b)
        | Inst::IntMul(a, b)
        | Inst::FloatMul(a, b)
        | Inst::FloatDiv(a, b)
        | Inst::IntMod(a, b)
        | Inst::FloatMod(a, b)
        | Inst::IntLt(a, b)
        | Inst::UintLt(a, b)
        | Inst::FloatLt(a, b)
        | Inst::IntLeq(a, b)
        | Inst::UintLeq(a, b)
        | Inst::FloatLeq(a, b)
        | Inst::IntGt(a, b)
        | Inst::UintGt(a, b)
        | Inst::FloatGt(a, b)
        | Inst::IntGeq(a, b)
        | Inst::UintGeq(a, b)
        | Inst::FloatGeq(a, b)
        | Inst::Eq(a, b)
        | Inst::NotEq(a, b) => vec![(a, 0), (b, 1)],
        Inst::Neg(x) | Inst::Not(x) => vec![(x, 1)],
        Inst::UintMul(..) => uniform(8),
        Inst::UintShr(..) => uniform(5),
        Inst::UintBitcastInt(_) => uniform(3),
        _ => uniform(2 + instrument),
    }
}

/// Give a register to every folded operand that would nest its user's
/// expression past [`skull_mir::lua_nesting::EXPRESSION_LEVEL_BUDGET`] LuaJIT
/// syntax levels. Folding is otherwise unbounded: a wide value interpolation
/// folds into one `..` run (a level per operand) and a run of `total += x`
/// statements into one expression with a parenthesis per statement, and the
/// chunk then fails to load. Values are visited in definition order, so every
/// folded operand is already within the budget; a value over it unfolds the
/// operands that put it there, which restores the bound without touching any
/// expression that already fits. An unfolded value is evaluated at its own
/// definition, which is the order the MIR states.
pub(crate) fn bound_fold_levels(func: &MirFunc, scope: &[Idx<Block>], inlinable: &mut [bool]) {
    let budget = skull_mir::lua_nesting::EXPRESSION_LEVEL_BUDGET;
    // Levels a value costs where it is rendered as an operand; 0 for a register.
    let mut levels = vec![0u32; func.insts.len()];
    let measure = |inlinable: &[bool], levels: &[u32], id: Idx<Inst>| -> u32 {
        match func.insts[id] {
            Inst::Load(_) => 0,
            // Rendered as its operand alone.
            Inst::IntToFloat(x) => levels[raw(x) as usize],
            _ => {
                let deepest = fold_operand_levels(func, inlinable, id)
                    .iter()
                    .map(|&(u, at)| at + levels[raw(u) as usize])
                    .max()
                    .unwrap_or(0);
                1 + deepest
            }
        }
    };
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            if !inlinable[k] {
                continue;
            }
            levels[k] = measure(inlinable, &levels, id);
            if levels[k] <= budget {
                continue;
            }
            // A run too long by itself: its folded halves become registers.
            if let Inst::StringConcat(a, b) = func.insts[id] {
                for half in [a, b] {
                    let h = raw(half) as usize;
                    if inlinable[h] && matches!(func.insts[half], Inst::StringConcat(..)) {
                        inlinable[h] = false;
                        levels[h] = 0;
                    }
                }
            }
            for (u, at) in fold_operand_levels(func, inlinable, id) {
                let x = raw(u) as usize;
                if inlinable[x] && 1 + at + levels[x] > budget && always_inlinable(&func.insts[u]) {
                    inlinable[x] = false;
                    levels[x] = 0;
                }
            }
            levels[k] = measure(inlinable, &levels, id);
        }
    }
}
