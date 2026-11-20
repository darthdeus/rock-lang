use super::*;

const NUMERIC_LATCH_LEN: usize = 4;

/// The upper bound of a counted loop: the RHS of the header `i < HI` compare.
pub(crate) enum HiLimit {
    /// A precomputed value or integer literal.
    Value(Idx<Inst>),
    /// An invariant expression from the header.
    Expr(Idx<Inst>),
    /// An invariant local loaded in the header.
    Cell(Idx<Cell>),
}

pub(crate) enum ForControl {
    Numeric {
        slot: Idx<Cell>,
        hi: HiLimit,
        inclusive: bool,
    },
    Map {
        key: Idx<Cell>,
        value: Idx<Cell>,
        table: Idx<Inst>,
    },
}

/// A native Lua `for`. Numeric loops subsume a separate increment latch;
/// map loops use their header as the continue target and have no latch work.
pub(crate) struct ForLoop {
    pub(crate) control: ForControl,
    pub(crate) body_entry: Idx<Block>,
    pub(crate) latch: Idx<Block>,
    pub(crate) exit: Idx<Block>,
    /// The natural-loop body blocks (excludes header + latch).
    pub(crate) region: HashSet<Idx<Block>>,
}

impl ForLoop {
    pub(crate) fn control_locals(&self) -> usize {
        match self.control {
            ForControl::Numeric { .. } => 4,
            ForControl::Map { .. } => 5,
        }
    }

    pub(crate) fn latch_body_len(&self, func: &MirFunc) -> usize {
        match self.control {
            ForControl::Numeric { .. } => func.blocks[self.latch].insts.len() - NUMERIC_LATCH_LEN,
            ForControl::Map { .. } => 0,
        }
    }
}

/// Instructions replaced by a native Lua `for`'s head and increment.
pub(crate) fn for_control_insts<'a>(
    func: &'a MirFunc,
    loops: &'a HashMap<Idx<Block>, ForLoop>,
) -> impl Iterator<Item = Idx<Inst>> + 'a {
    loops.iter().flat_map(move |(&header, cl)| {
        let latch = if cl.latch == header {
            &[]
        } else {
            &func.blocks[cl.latch].insts[cl.latch_body_len(func)..]
        };
        func.blocks[header].insts.iter().chain(latch).copied()
    })
}

/// Every recognized loop in `func`, keyed by header block. Coroutine cursors
/// stay in frame cells, since native Lua `for` state cannot survive a yield.
pub(crate) fn recognize_for_loops(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    frame: Option<&FrameCtx>,
    inlinable: &[bool],
) -> HashMap<Idx<Block>, ForLoop> {
    let mut loops = HashMap::default();
    let latched = rpo.iter().any(|&p| {
        matches!(func.blocks[p].term, Term::Goto(h) if latch_slot(func, p, h).is_some())
            || func.blocks[p]
                .insts
                .iter()
                .any(|&i| matches!(func.insts[i], Inst::MapNext { .. }))
    });
    if !latched {
        return loops;
    }
    let (all_preds, goto_preds) = predecessors(func, rpo);
    for &h in rpo {
        if let Some(cl) = try_match_map(func, h, &all_preds, frame)
            .or_else(|| try_match_loop(func, h, &all_preds, &goto_preds, frame, inlinable))
        {
            loops.insert(h, cl);
        }
    }
    loops
}

fn try_match_loop(
    func: &MirFunc,
    h: Idx<Block>,
    all_preds: &PredMap,
    goto_preds: &PredMap,
    frame: Option<&FrameCtx>,
    inlinable: &[bool],
) -> Option<ForLoop> {
    let Term::Branch {
        cond,
        then_bb,
        else_bb,
    } = func.blocks[h].term
    else {
        return None;
    };
    let gp = goto_preds.get(&h)?;
    if gp.len() != 2 || all_preds.get(&h).map_or(0, |v| v.len()) != 2 {
        return None;
    }
    let (latch, slot, preheader) = match (latch_slot(func, gp[0], h), latch_slot(func, gp[1], h)) {
        (Some(slot), None) => (gp[0], slot, gp[1]),
        (None, Some(slot)) => (gp[1], slot, gp[0]),
        _ => return None,
    };
    let (hi, inclusive, limit_cells) = match_header(func, h, cond, slot, inlinable)?;
    if limit_cells.contains(&slot)
        || std::iter::once(slot)
            .chain(limit_cells.iter().copied())
            .any(|c| func.cells[c].borrowed || frame.is_some_and(|f| f.fields.contains(&c)))
    {
        return None;
    }
    let region = compute_region(all_preds, then_bb, h, latch)?;
    // The post-for edge must leave the loop. A short-circuit condition can
    // route the false arm through a join that is still inside the body.
    if else_bb == h || else_bb == latch || region.contains(&else_bb) || region.contains(&preheader)
    {
        return None;
    }
    let small_start = func.blocks[latch].insts.len() != NUMERIC_LATCH_LEN
        || !inclusive && matches!(hi, HiLimit::Expr(_));
    let init = counter_init(func, slot, &limit_cells, h, latch, &region, small_start)?;
    if !resets_on_reentry(all_preds, h, preheader, init) {
        return None;
    }
    Some(ForLoop {
        control: ForControl::Numeric {
            slot,
            hi,
            inclusive,
        },
        body_entry: then_bb,
        latch,
        exit: else_bb,
        region,
    })
}

/// The latch ends in `vj = load slot; one = 1; vinc = vj + one; store slot = vinc`.
fn latch_slot(func: &MirFunc, p: Idx<Block>, header: Idx<Block>) -> Option<Idx<Cell>> {
    if func.blocks[p].term != Term::Goto(header) {
        return None;
    }
    let (_, ins) = func.blocks[p]
        .insts
        .split_last_chunk::<NUMERIC_LATCH_LEN>()?;
    let (
        Inst::Load(slot),
        Inst::Const(Lit::Int(1)),
        Inst::IntAdd(a, b),
        Inst::Store { cell, value },
    ) = (
        &func.insts[ins[0]],
        &func.insts[ins[1]],
        &func.insts[ins[2]],
        &func.insts[ins[3]],
    )
    else {
        return None;
    };
    (*a == ins[0] && *b == ins[1] && *cell == *slot && *value == ins[2]).then_some(*slot)
}

fn match_header(
    func: &MirFunc,
    h: Idx<Block>,
    cond: Idx<Inst>,
    slot: Idx<Cell>,
    inlinable: &[bool],
) -> Option<(HiLimit, bool, Vec<Idx<Cell>>)> {
    let (lhs, rhs, inclusive) = match &func.insts[cond] {
        Inst::IntLt(a, b) => (*a, *b, false),
        Inst::IntLeq(a, b) => (*a, *b, true),
        _ => return None,
    };
    let insts = &func.blocks[h].insts;
    if !matches!(&func.insts[lhs], Inst::Load(s) if *s == slot) || !insts.contains(&lhs) {
        return None;
    }
    let extra = insts
        .iter()
        .copied()
        .filter(|&i| i != lhs && i != cond && i != rhs);
    let simple = extra.clone().next().is_none();
    if !insts.contains(&rhs) {
        return simple.then_some((HiLimit::Value(rhs), inclusive, vec![]));
    }
    let mut cells = Vec::new();
    let hi = match func.insts[rhs] {
        Inst::Load(c) if simple => {
            cells.push(c);
            HiLimit::Cell(c)
        }
        Inst::Const(Lit::Int(n)) if simple && (inclusive || n > -(1_i64 << 53)) => {
            HiLimit::Value(rhs)
        }
        Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) => {
            for id in extra.chain([rhs]) {
                if !inlinable[raw(id) as usize] {
                    return None;
                }
                match func.insts[id] {
                    Inst::Load(c) => cells.push(c),
                    Inst::Const(Lit::Int(_))
                    | Inst::IntAdd(..)
                    | Inst::IntSub(..)
                    | Inst::IntMul(..) => {}
                    _ => return None,
                }
            }
            HiLimit::Expr(rhs)
        }
        _ => return None,
    };
    Some((hi, inclusive, cells))
}

/// The natural loop of `latch -> header`: blocks that reach the latch without
/// crossing the header, so a `break` path stays outside and exits via goto.
/// `None` if the body can't iterate or the loop is irreducible.
fn compute_region(
    all_preds: &PredMap,
    body_entry: Idx<Block>,
    header: Idx<Block>,
    latch: Idx<Block>,
) -> Option<HashSet<Idx<Block>>> {
    let mut natural: HashSet<Idx<Block>> = HashSet::default();
    natural.insert(latch);
    let mut stack = vec![latch];
    while let Some(b) = stack.pop() {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && natural.insert(p) {
                    stack.push(p);
                }
            }
        }
    }
    if !natural.contains(&body_entry) {
        return None;
    }
    natural.remove(&latch);
    // A region block may be entered only from the region, header or latch;
    // else a goto jumps into the middle of the `for`.
    for &b in &natural {
        if let Some(ps) = all_preds.get(&b) {
            for &p in ps {
                if p != header && p != latch && !natural.contains(&p) {
                    return None;
                }
            }
        }
    }
    Some(natural)
}

/// A Lua numeric `for` shadows the counter and snapshots the limit.
fn counter_init(
    func: &MirFunc,
    slot: Idx<Cell>,
    limit_cells: &[Idx<Cell>],
    header: Idx<Block>,
    latch: Idx<Block>,
    region: &HashSet<Idx<Block>>,
    small_start: bool,
) -> Option<Idx<Block>> {
    let latch_body_len = func.blocks[latch].insts.len() - NUMERIC_LATCH_LEN;
    let mut init = None;
    for (bid, b) in func.blocks.iter() {
        let inside = bid == header || bid == latch || region.contains(&bid);
        let insts = if bid == latch {
            &b.insts[..latch_body_len]
        } else {
            &b.insts
        };
        for &i in insts {
            let inst = &func.insts[i];
            if matches!(inst, Inst::Load(c) if *c == slot && !inside) {
                return None;
            }
            let mut sound = true;
            inst.cell_writes_each(&mut |cell| {
                if inside && limit_cells.contains(&cell) {
                    sound = false;
                }
                if cell == slot {
                    // Below -2^53, subtracting one may round back to the exclusive limit.
                    sound &= !inside && matches!(inst, Inst::Store { value, .. }
                        if !small_start || matches!(func.insts[*value], Inst::Const(Lit::Int(0..=2_147_483_647))));
                    sound &= init.replace(bid).is_none();
                }
            });
            if !sound {
                return None;
            }
        }
    }
    init
}

fn resets_on_reentry(
    preds: &PredMap,
    header: Idx<Block>,
    preheader: Idx<Block>,
    init: Idx<Block>,
) -> bool {
    if preheader == init {
        return true;
    }
    // The numeric for does not write its final counter back to the outer cell.
    let mut seen = HashSet::default();
    let mut work = vec![preheader];
    while let Some(block) = work.pop() {
        if block == header {
            return false;
        }
        if block != init && seen.insert(block) {
            work.extend(preds.get(&block).into_iter().flatten().copied());
        }
    }
    true
}

// A map loop has no separate increment: the header is also its continue
// target. Its nil-initialized cursor and values are private to this walk.
fn try_match_map(
    func: &MirFunc,
    header: Idx<Block>,
    preds: &PredMap,
    frame: Option<&FrameCtx>,
) -> Option<ForLoop> {
    if frame.is_some() {
        return None;
    }
    let Term::Branch {
        cond,
        then_bb: exit,
        else_bb: body_entry,
    } = func.blocks[header].term
    else {
        return None;
    };
    let insts = &func.blocks[header].insts;
    let &next = insts.first()?;
    let Inst::MapNext {
        key_cell: key,
        val_cell: value,
        table,
    } = func.insts[next]
    else {
        return None;
    };
    let Inst::Eq(a, b) = func.insts[cond] else {
        return None;
    };
    let is_key = |i| matches!(func.insts[i], Inst::Load(c) if c == key);
    let is_nil = |i| matches!(func.insts[i], Inst::Const(Lit::Nil));
    if !(is_key(a) && is_nil(b) || is_key(b) && is_nil(a)) {
        return None;
    }
    if insts
        .iter()
        .any(|&i| i != next && i != cond && !is_key(i) && !is_nil(i))
    {
        return None;
    }
    // Exactly one store initializes the cursor, and it must be nil. The
    // generic-for state then advances independently of body-local kills.
    let mut preheader = None;
    for (bid, block) in func.blocks.iter() {
        for &i in &block.insts {
            if let Inst::Store { cell, value: v } = func.insts[i]
                && cell == key
            {
                if preheader.is_some() || !is_nil(v) || block.term != Term::Goto(header) {
                    return None;
                }
                preheader = Some(bid);
            }
        }
    }
    let preheader = preheader?;
    let mut region = HashSet::default();
    let mut stack: Vec<_> = preds
        .get(&header)?
        .iter()
        .copied()
        .filter(|&b| b != preheader)
        .collect();
    while let Some(b) = stack.pop() {
        if b == header || !region.insert(b) {
            continue;
        }
        for &p in preds.get(&b).into_iter().flatten() {
            if p != header {
                stack.push(p);
            }
        }
    }
    if !region.contains(&body_entry)
        || region.contains(&preheader)
        || region.contains(&exit)
        || region.iter().any(|b| preds.get(b).is_none())
    {
        return None;
    }
    for (bid, block) in func.blocks.iter() {
        for &i in &block.insts {
            let mut sound = true;
            func.insts[i].cell_writes_each(&mut |c| {
                if (c == key || c == value) && i != next && !(c == key && bid == preheader) {
                    sound = false;
                }
            });
            if !sound {
                return None;
            }
            if matches!(func.insts[i], Inst::Load(c) if c == key || c == value)
                && bid != header
                && !region.contains(&bid)
            {
                return None;
            }
        }
    }
    Some(ForLoop {
        control: ForControl::Map { key, value, table },
        body_entry,
        latch: header,
        exit,
        region,
    })
}
