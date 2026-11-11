//! Recognition of numeric and map loops for native Lua `for` emission.

use super::*;

/// The upper bound of a counted loop — the RHS of the header `i < HI` compare.
pub(crate) enum HiLimit {
    /// A value computed in the preheader (e.g. `#arr`); live through the header.
    Value(Idx<Inst>),
    /// A bound cell loaded in the header (`for i in lo..hi`'s `hi`).
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
}

/// Every recognized loop in `func`, keyed by header block. Coroutine cursors
/// stay in frame cells, since native Lua `for` state cannot survive a yield.
pub(crate) fn recognize_for_loops(
    func: &MirFunc,
    rpo: &[Idx<Block>],
    frame: Option<&FrameCtx>,
) -> HashMap<Idx<Block>, ForLoop> {
    let mut loops = HashMap::default();
    // Avoid predecessor maps for bodies without either kind of loop.
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
            .or_else(|| try_match_loop(func, h, &all_preds, &goto_preds, frame))
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
) -> Option<ForLoop> {
    let Term::Branch {
        cond,
        then_bb,
        else_bb,
    } = func.blocks[h].term
    else {
        return None;
    };
    // Header is entered by exactly the preheader + the latch, both via goto.
    let gp = goto_preds.get(&h)?;
    if gp.len() != 2 || all_preds.get(&h).map_or(0, |v| v.len()) != 2 {
        return None;
    }
    // Exactly one goto-pred is a pure-increment latch; the other is the preheader.
    let mut latch = None;
    let mut slot = None;
    for &p in gp {
        if let Some(s) = latch_slot(func, p, h) {
            latch = Some(p);
            slot = Some(s);
        }
    }
    let (latch, slot) = (latch?, slot?);
    if frame.is_some_and(|f| f.fields.contains(&slot)) {
        return None;
    }
    let (hi, inclusive) = match_header(func, h, cond, slot)?;
    let region = compute_region(all_preds, then_bb, h, latch)?;
    // The post-for edge must leave the loop. A short-circuit condition can
    // route the false arm through a join that is still inside the body.
    if else_bb == h || else_bb == latch || region.contains(&else_bb) {
        return None;
    }
    if !slot_sound(func, slot, h, latch, &region) {
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

/// `Some(slot)` iff block `p` is a pure 4-inst increment latch jumping to
/// `header`: `vj = load slot; one = 1; vinc = vj + one; store slot = vinc`. The
/// strictness is load-bearing — a loop whose increment is fused into the body
/// (no dedicated latch) would otherwise match and double-increment.
fn latch_slot(func: &MirFunc, p: Idx<Block>, header: Idx<Block>) -> Option<Idx<Cell>> {
    if func.blocks[p].term != Term::Goto(header) {
        return None;
    }
    let ins = &func.blocks[p].insts;
    if ins.len() != 4 {
        return None;
    }
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

/// The header must be exactly `load slot; [load hi_cell;] (slot < HI | slot <= HI)`
/// — the counter load(s) feeding the compare, nothing else (no side effects).
fn match_header(
    func: &MirFunc,
    h: Idx<Block>,
    cond: Idx<Inst>,
    slot: Idx<Cell>,
) -> Option<(HiLimit, bool)> {
    let (lhs, rhs, inclusive) = match &func.insts[cond] {
        Inst::IntLt(a, b) => (*a, *b, false),
        Inst::IntLeq(a, b) => (*a, *b, true),
        _ => return None,
    };
    let insts = &func.blocks[h].insts;
    // LHS is a Load(slot) present in the header.
    if !matches!(&func.insts[lhs], Inst::Load(s) if *s == slot) || !insts.contains(&lhs) {
        return None;
    }
    // RHS is either a bound cell loaded in the header, or a preheader value.
    let rhs_is_cell_load = matches!(&func.insts[rhs], Inst::Load(_)) && insts.contains(&rhs);
    let allowed: HashSet<Idx<Inst>> = if rhs_is_cell_load {
        [lhs, rhs, cond].into_iter().collect()
    } else {
        [lhs, cond].into_iter().collect()
    };
    if insts.iter().any(|i| !allowed.contains(i)) {
        return None;
    }
    let hi = match &func.insts[rhs] {
        Inst::Load(c) if rhs_is_cell_load => HiLimit::Cell(*c),
        _ => HiLimit::Value(rhs),
    };
    Some((hi, inclusive))
}

/// The natural loop of the back-edge `latch -> header`: blocks that reach the
/// latch WITHOUT crossing the header (backward reach over preds), NOT
/// "everything reachable from body_entry" — a `break`/early-return path doesn't
/// reach the latch, so it stays OUTSIDE the loop and exits via goto. Returns
/// `None` if the body can't iterate, or the loop is multi-entry (irreducible).
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
    // Reducible / single-entry: a region block may be entered only from within
    // region ∪ {header, latch} — else a goto jumps into the middle of the `for`.
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

/// The counter slot must behave like a loop-scoped `for` variable: stored
/// EXACTLY twice (preheader init + latch increment — so it's never reassigned in
/// the body), and never read outside the loop region (incl. closures, whose body
/// blocks are outside the region — a captured counter can't be the for-variable).
fn slot_sound(
    func: &MirFunc,
    slot: Idx<Cell>,
    header: Idx<Block>,
    latch: Idx<Block>,
    region: &HashSet<Idx<Block>>,
) -> bool {
    let in_loop: HashSet<Idx<Block>> = region.iter().copied().chain([header, latch]).collect();
    let mut stores = 0usize;
    for (bid, b) in func.blocks.iter() {
        let inside = in_loop.contains(&bid);
        for &i in &b.insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == slot => stores += 1,
                Inst::Load(c) if *c == slot && !inside => return false,
                Inst::MapNext {
                    key_cell, val_cell, ..
                } if *key_cell == slot || *val_cell == slot => return false,
                _ => {}
            }
        }
    }
    stores == 2
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
