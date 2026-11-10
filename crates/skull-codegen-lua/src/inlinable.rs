//! Which instructions render inline at their use, and fold-depth bounds.

use super::*;

pub(crate) use skull_mir::sinking::always_inlinable;

/// Cells written exactly once in `scope`, as a multi-return destination, and
/// read only after that call in the same block. A load of one is the call's
/// result local itself, like a parameter load: without this every leaf of a
/// wide value returned by a call occupied two locals (the result and a copy),
/// which is what pushed functions holding a few wide values past LuaJIT's
/// 250-slot frame limit. No later write can intervene, because the one write
/// dominates every read. Narrow receives qualify too.
fn single_receive_cells(func: &MirFunc, scope: &[Idx<Block>], pinned: &[bool]) -> Vec<bool> {
    // cell -> (defs, the receive's block and position). Only a receive records
    // a position, so a cell qualifies iff its one def is one.
    let mut defs: Vec<(u32, Option<(Idx<Block>, usize)>)> = vec![(0, None); func.cells.len()];
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            match &func.insts[id] {
                Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => {
                    for cell in dests {
                        let entry = &mut defs[raw(*cell) as usize];
                        entry.0 += 1;
                        entry.1 = Some((b, pos));
                    }
                }
                // A placeholder the bound substitution replaces; never a receive.
                Inst::CallBoundMulti { dests, .. } => {
                    for cell in dests {
                        defs[raw(*cell) as usize].0 += 1;
                    }
                }
                other => regalloc::cell_defs(other, &mut |cell| {
                    defs[raw(cell) as usize].0 += 1;
                }),
            }
        }
    }
    let captured = |cell: Idx<Cell>| {
        func.closure_env_plans
            .values()
            .any(|plan| plan.captures_cell(cell) || plan.env_cell() == Some(cell))
    };
    let mut out = vec![false; func.cells.len()];
    let mut any = false;
    for (cell, _) in func.cells.iter() {
        let (count, at) = defs[raw(cell) as usize];
        if count == 1 && at.is_some() && !pinned[raw(cell) as usize] && !captured(cell) {
            out[raw(cell) as usize] = true;
            any = true;
        }
    }
    if !any {
        return out;
    }
    for &b in scope {
        for (pos, &id) in func.blocks[b].insts.iter().enumerate() {
            regalloc::cell_uses(&func.insts[id], &mut |cell| {
                if !out[raw(cell) as usize] {
                    return;
                }
                let Some((_, Some((def_block, def_pos)))) = defs.get(raw(cell) as usize) else {
                    return;
                };
                if *def_block != b || *def_pos >= pos || !matches!(func.insts[id], Inst::Load(_)) {
                    out[raw(cell) as usize] = false;
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
    let mut immutable_params = vec![false; func.cells.len()];
    for &cell in params {
        immutable_params[raw(cell) as usize] = true;
    }
    for &b in scope {
        let br = raw(b);
        for &id in &func.blocks[b].insts {
            func.insts[id].cell_writes_each(&mut |cell| {
                immutable_params[raw(cell) as usize] = false;
            });
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
                Inst::Load(cell)
                    if immutable_params[raw(*cell) as usize] || received[raw(*cell) as usize]
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

/// Whether `consumer` renders each of its operands exactly once, so a folded
/// call among them still runs once. Shapes that spell an operand several
/// times (`UintMul`, a `Switch` ladder, a guarded store) are absent.
fn spells_operands_once(consumer: &Inst) -> bool {
    matches!(
        consumer,
        Inst::Store { .. }
            | Inst::StoreGlobal { .. }
            | Inst::Call { .. }
            | Inst::CallValue { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValueMulti { .. }
            | Inst::Field { .. }
            | Inst::TupleIndex { .. }
            | Inst::IntAdd(..)
            | Inst::IntSub(..)
            | Inst::IntMul(..)
            | Inst::FloatAdd(..)
            | Inst::FloatSub(..)
            | Inst::FloatMul(..)
            | Inst::FloatDiv(..)
            | Inst::IntLt(..)
            | Inst::IntLeq(..)
            | Inst::IntGt(..)
            | Inst::IntGeq(..)
            | Inst::FloatLt(..)
            | Inst::FloatLeq(..)
            | Inst::FloatGt(..)
            | Inst::FloatGeq(..)
            | Inst::Eq(..)
            | Inst::NotEq(..)
            | Inst::Not(_)
            | Inst::StringConcat(..)
    )
}

/// Fold a read or call into its one use in the same block, so its value
/// never occupies a Lua local of its own: no register, no move into the
/// cell or argument slot that consumes it, and no kill afterwards. A cell
/// load qualifies when no write to the cell can run before the consumer
/// ([`keep_reads_ordered`]); a projection or call is a throwing
/// instruction whose order with other effects [`preserve_effect_order`]
/// keeps. Returns the newly folded instructions.
///
/// [`preserve_effect_order`]: skull_mir::sinking::preserve_effect_order
#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_single_use_reads(
    ws: &Workspace,
    func: &MirFunc,
    scope: &[Idx<Block>],
    inlinable: &mut [bool],
    pinned: &[bool],
    suppressed: &[bool],
    call_arg_forwards: &CallArgForwards,
    line_marked: bool,
) -> Vec<bool> {
    const NONE: u32 = u32::MAX;
    // Under line directives a statement reports its own source line, so a
    // value folded into a statement must come from that line.
    let same_line = |a: Idx<Inst>, b: u32| {
        let b = Idx::from_raw(b.into());
        match (func.inst_spans.get(a), func.inst_spans.get(b)) {
            (Some(x), Some(y)) => x.file == y.file && x.line_range().0 == y.line_range().0,
            _ => true,
        }
    };
    let ni = func.insts.len();
    let mut use_count = vec![0u32; ni];
    let mut use_block = vec![NONE; ni];
    // The consumer of a single use: an instruction, or `NONE` for a terminator.
    let mut user = vec![NONE; ni];
    let mut term_user_once = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            regalloc::inst_uses(&func.insts[id], &mut |u| {
                let k = raw(u) as usize;
                use_count[k] += 1;
                use_block[k] = raw(b);
                user[k] = raw(id);
            });
        }
        let term = &func.blocks[b].term;
        regalloc::term_uses(term, &mut |u| {
            let k = raw(u) as usize;
            use_count[k] += 1;
            use_block[k] = raw(b);
            user[k] = NONE;
            term_user_once[k] = matches!(term, Term::Return(_) | Term::Branch { .. });
        });
    }
    // Operands that must keep a register: one that would open its statement
    // (a parenthesized operand there is ambiguous Lua after an expression
    // line), and the operands of a push, which writes the length before it
    // evaluates the pushed value.
    let mut pinned_operand = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let mut pin = |v: Idx<Inst>| pinned_operand[raw(v) as usize] = true;
            match &func.insts[id] {
                Inst::StoreField { base, .. } | Inst::StoreIndex { base, .. } => pin(*base),
                Inst::CallValueMulti { dests, callee, .. } if dests.is_empty() => pin(*callee),
                Inst::ArrayPush { arr, value, .. } => {
                    pin(*arr);
                    pin(*value);
                }
                _ => {}
            }
        }
    }
    let mut newly = vec![false; ni];
    for &b in scope {
        for &id in &func.blocks[b].insts {
            let k = raw(id) as usize;
            if inlinable[k]
                || pinned_operand[k]
                || suppressed[k]
                || call_arg_forwards.get(id).is_some()
                || use_count[k] != 1
                || use_block[k] != raw(b)
            {
                continue;
            }
            let ok = match &func.insts[id] {
                Inst::Load(cell) => !pinned[raw(*cell) as usize],
                Inst::Field { .. }
                | Inst::TupleIndex { .. }
                | Inst::Index { .. }
                | Inst::VecCheckedIndex { .. }
                | Inst::Len { .. } => true,
                inst @ (Inst::Call { .. } | Inst::CallValue { .. }) => {
                    // An inline(lua) callee may be spliced, which needs a
                    // register to receive its result.
                    let splices = matches!(inst, Inst::Call { func: f, .. }
                        if func_of(ws, *f).inline_body.is_some());
                    !splices
                        && match user[k] {
                            NONE => term_user_once[k],
                            u => spells_operands_once(&func.insts[Idx::from_raw(u.into())]),
                        }
                }
                _ => false,
            };
            // A call folded into another call's operands stacks its frame on
            // the outer one's. One level keeps `f(g(x))` while a chain cannot
            // grow the frame past LuaJIT's slot cap.
            let mut nested = false;
            if matches!(func.insts[id], Inst::Call { .. } | Inst::CallValue { .. }) {
                func.insts[id].uses_each(&mut |v| {
                    nested |= frame_limits::folded_call_within(func, inlinable, v);
                });
            }
            let misplaced = line_marked && user[k] != NONE && !same_line(id, user[k]);
            if ok && !nested && !misplaced {
                inlinable[k] = true;
                newly[k] = true;
            }
        }
    }
    newly
}

/// The value a projection indexes, when the projection can fail only by
/// indexing a non-table, which LuaJIT reports by the indexed operand alone.
fn indexed_base(inst: &Inst) -> Option<Idx<Inst>> {
    match *inst {
        Inst::Field { base, .. } | Inst::TupleIndex { base, .. } => Some(base),
        Inst::Len {
            base,
            kind:
                ContainerKind::Vec
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_)
                | ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferValue(_),
        } => Some(base),
        _ => None,
    }
}

/// Two projections of the same operand fail together with one message, so
/// either may be evaluated first: `f(p.x, p.y)` needs no register for `p.x`.
/// Separate loads of one cell count as one operand: only other projections
/// may stand between the two, so nothing writes the cell in between.
pub(crate) fn same_base_projections(func: &MirFunc) -> impl Fn(&Inst, &Inst) -> bool + '_ {
    let operand = |base: Idx<Inst>| match func.insts[base] {
        Inst::Load(cell) => (true, cell.into_raw().into_u32()),
        _ => (false, base.into_raw().into_u32()),
    };
    move |folded, between| {
        indexed_base(folded)
            .is_some_and(|a| indexed_base(between).is_some_and(|b| operand(a) == operand(b)))
    }
}

/// Unfold a newly folded cell load whose consumer runs after a write to the
/// same cell: the folded read would see the new value. Runs after every
/// other folding decision, since unfolding a consumer only moves its
/// operands' evaluation earlier.
pub(crate) fn keep_reads_ordered(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inlinable: &mut [bool],
    newly: &[bool],
) {
    let ni = func.insts.len();
    // Position in its block of the statement that finally evaluates a value.
    let mut eval = vec![0usize; ni];
    let mut user = vec![u32::MAX; ni];
    for &b in scope {
        let insts = &func.blocks[b].insts;
        for &id in insts {
            regalloc::inst_uses(&func.insts[id], &mut |u| user[raw(u) as usize] = raw(id));
        }
        regalloc::term_uses(&func.blocks[b].term, &mut |u| {
            user[raw(u) as usize] = u32::MAX
        });
        for (pos, &id) in insts.iter().enumerate().rev() {
            let k = raw(id) as usize;
            eval[k] = if inlinable[k] {
                match user[k] {
                    u32::MAX => insts.len(),
                    u => eval[u as usize],
                }
            } else {
                pos
            };
        }
        for (pos, &id) in insts.iter().enumerate() {
            let k = raw(id) as usize;
            if !newly[k] || !inlinable[k] {
                continue;
            }
            let end = eval[k].min(insts.len());
            // A call may write a mutable global, so it must not move past a
            // read of one.
            if matches!(func.insts[id], Inst::Call { .. } | Inst::CallValue { .. })
                && insts[pos + 1..end]
                    .iter()
                    .any(|&at| matches!(func.insts[at], Inst::Global(_)))
            {
                inlinable[k] = false;
                continue;
            }
            let Inst::Load(cell) = func.insts[id] else {
                // A push writes the container's length before it evaluates
                // the pushed value, so no memory read may be deferred into it.
                if insts
                    .get(eval[k])
                    .is_some_and(|&at| matches!(func.insts[at], Inst::ArrayPush { .. }))
                {
                    inlinable[k] = false;
                }
                continue;
            };
            let mut written = false;
            for &between in &insts[pos + 1..end] {
                func.insts[between].cell_writes_each(&mut |c| written |= c == cell);
            }
            if written {
                inlinable[k] = false;
            }
        }
    }
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
pub(crate) fn bound_fold_levels(
    func: &MirFunc,
    scope: &[Idx<Block>],
    inlinable: &mut [bool],
    unfoldable: &[bool],
) {
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
                if inlinable[x]
                    && 1 + at + levels[x] > budget
                    && (always_inlinable(&func.insts[u]) || unfoldable[x])
                {
                    inlinable[x] = false;
                    levels[x] = 0;
                }
            }
            levels[k] = measure(inlinable, &levels, id);
        }
    }
}
