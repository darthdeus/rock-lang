//! FORL recovery: rewrite the desugared `for x in <user iter>` loop (after
//! inlining + SROA) into the exact goto-loop shape `skull-codegen-lua`'s
//! `CountedLoop` detector recognizes, so it prints as a native Lua numeric
//! `for` (LuaJIT FORL bytecode: hoisted bounds check, counted back-edge,
//! int<->double transition tolerance). The goto-LOOP form of the same loop
//! measures 1.8x slower on a type-stable 2M-element sum
//! (`docs/iterator-implementation-plan.md` slice 4.5).
//!
//! Fires ONLY on loops tagged by `lower_for_user_iter`
//! (`MirFunc::for_iter_headers`). The one observable change is bound SNAPSHOT:
//! the per-iteration bound read (e.g. the inlined `it.vec.len`) moves to the
//! preheader and is evaluated once per loop. That diverges only when the body
//! mutates the collection being iterated, which is recorded UB
//! (`docs/iterator-desugaring-investigation.md` resolved decisions: "iterator
//! invalidation: ignored"). The sanction is exactly why the pass is tag-gated
//! -- a user-written `while i < xs.len` must keep re-reading its bound.
//!
//! Post-inline shape matched (H = the tagged head):
//! ```text
//! P:  ...; store slot = <init>; goto H       -- unique non-back-edge pred
//! H:  goto.. T                               -- back-edge/continue target
//! T:  l = load slot; <bound chain>; c = cmp(l, B); branch c -> D | Y
//! D:  store flag = false; store elem = <const|phantom()>; goto.. J
//! Y:  <elem reads>; store slot = slot + 1; store flag = true; ..; goto.. J
//! J:  f = load flag; branch f -> BODY | EXIT
//! ```
//! Rewrite: hoist the bound chain T->P; T becomes `load slot; IntLt/IntLeq`
//! branching yield-arm/EXIT; Y drops the increment and jumps straight to BODY;
//! a fresh 4-inst latch L (`load; 1; add; store; goto T`) takes every former
//! back-edge/continue; the preheader targets T; H/D/J are cleared (the
//! detector's `slot_sound` scans ALL arena blocks, incl. unreachable ones).
//!
//! Continue stays correct: the old code increments PRE-body (inside the
//! inlined `next`), so `continue -> H` re-tested an already-advanced slot; the
//! new code strips that increment and `continue -> L` performs it instead --
//! same test values, one increment per iteration either way. The in-body slot
//! value differs by one, which is unobservable because the matcher rejects any
//! slot read outside T's compare and Y's pre-increment reads.
//!
//! Every bail leaves the loop untouched. The rewrite itself is
//! semantics-preserving even when the downstream detector then declines
//! (worst case: a correct goto loop with a dedicated latch).

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use skull_mir::mir::*;

/// Rewrite every tagged, matching desugared-iterator loop in `func` into the
/// codegen `CountedLoop` shape. Run after `inline_functions` + `cleanup` (the
/// slot only exists as a scalar cell once SROA has peeled the cursor struct).
pub fn recover_counted_loops(ws: &Workspace, func: &mut MirFunc) {
    if func.for_iter_headers.is_empty() {
        return;
    }
    let headers = func.for_iter_headers.clone();
    let mut changed = false;
    for h in headers {
        if let Some(m) = try_match(ws, func, h) {
            rewrite(func, h, m);
            changed = true;
        }
    }
    if changed {
        crate::dce(func);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Cmp {
    Lt,
    Leq,
    Gt,
    Geq,
}

struct LoopMatch {
    /// Test block: holds the compare + branch (== the head when it has insts).
    t: Idx<Block>,
    /// Unique non-back-edge pred of the head; must end `Goto(head)`.
    p: Idx<Block>,
    /// Yield-path block (stores the increment + `flag = true`).
    y: Idx<Block>,
    /// T's branch arm on the yield side (pre-chase, kept as the `for` body entry).
    y_arm: Idx<Block>,
    body: Idx<Block>,
    exit: Idx<Block>,
    slot: Idx<Cell>,
    /// The non-slot compare operand (the bound value).
    bound: Idx<Inst>,
    /// Continue while `slot <= bound` (else `<`).
    inclusive: bool,
    /// Position of the increment `Store{slot}` in `y.insts`.
    incr_pos: usize,
    /// Bound-chain insts currently listed in T, in T order (to hoist).
    chain: Vec<Idx<Inst>>,
    /// Former back-edge preds of the head (body end + every `continue`).
    back: Vec<Idx<Block>>,
    /// Scaffolding to clear: head chain, done-path chain, join chain.
    clear: Vec<Idx<Block>>,
}

/// Follow a chain of empty single-goto blocks starting at `b` (inclusive),
/// collecting the empties into `hops`.
fn chase(func: &MirFunc, mut b: Idx<Block>, hops: &mut Vec<Idx<Block>>) -> Idx<Block> {
    let mut n = 0;
    while func.blocks[b].insts.is_empty() && n < 64 {
        match func.blocks[b].term {
            Term::Goto(t) => {
                hops.push(b);
                b = t;
                n += 1;
            }
            _ => break,
        }
    }
    b
}

fn targets(term: &Term, mut f: impl FnMut(Idx<Block>)) {
    match term {
        Term::Goto(t) => f(*t),
        Term::Branch { then_bb, else_bb, .. } => {
            f(*then_bb);
            f(*else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                f(*b);
            }
            f(*default);
        }
        Term::Yield { resume, .. } => f(*resume),
        Term::Return(_) | Term::Unreachable => {}
    }
}

fn retarget(term: &mut Term, from: Idx<Block>, to: Idx<Block>) {
    match term {
        Term::Goto(t) => {
            if *t == from {
                *t = to;
            }
        }
        Term::Branch { then_bb, else_bb, .. } => {
            if *then_bb == from {
                *then_bb = to;
            }
            if *else_bb == from {
                *else_bb = to;
            }
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases {
                if *b == from {
                    *b = to;
                }
            }
            if *default == from {
                *default = to;
            }
        }
        Term::Yield { resume, .. } => {
            if *resume == from {
                *resume = to;
            }
        }
        Term::Return(_) | Term::Unreachable => {}
    }
}

/// Whether any inst in `blocks` writes cell `c` (store, multi-call dest,
/// map-next out-cell, or lambda param rebinding).
fn cell_written_in(func: &MirFunc, c: Idx<Cell>, blocks: &HashSet<Idx<Block>>) -> bool {
    for &b in blocks {
        for &i in &func.blocks[b].insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == c => return true,
                Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. }
                    if dests.contains(&c) =>
                {
                    return true
                }
                Inst::MapNext { key_cell, val_cell, .. }
                    if *key_cell == c || *val_cell == c =>
                {
                    return true
                }
                Inst::Lambda { params, .. } if params.contains(&c) => return true,
                _ => {}
            }
        }
    }
    false
}

fn fn_name(ws: &Workspace, fr: FuncRef) -> &str {
    ws.module(fr.module).functions[fr.func].name.as_str()
}

fn try_match(ws: &Workspace, func: &MirFunc, h: Idx<Block>) -> Option<LoopMatch> {
    // Head -> test block through empty gotos.
    let mut head_chain = Vec::new();
    let t = chase(func, h, &mut head_chain);
    let Term::Branch { cond, then_bb, else_bb } = func.blocks[t].term else {
        return None;
    };

    // The compare, with the slot on either side.
    let (cmp, a, b) = match &func.insts[cond] {
        Inst::IntLt(a, b) => (Cmp::Lt, *a, *b),
        Inst::IntLeq(a, b) => (Cmp::Leq, *a, *b),
        Inst::IntGt(a, b) => (Cmp::Gt, *a, *b),
        Inst::IntGeq(a, b) => (Cmp::Geq, *a, *b),
        _ => return None,
    };

    // Chase both arms; the yield side is the one that stores the slot.
    let mut then_chain = Vec::new();
    let ca = chase(func, then_bb, &mut then_chain);
    let mut else_chain = Vec::new();
    let cb = chase(func, else_bb, &mut else_chain);
    let stores_cell = |blk: Idx<Block>, c: Idx<Cell>| {
        func.blocks[blk]
            .insts
            .iter()
            .any(|&i| matches!(&func.insts[i], Inst::Store { cell, .. } if *cell == c))
    };
    let mut pick: Option<(Idx<Inst>, Idx<Inst>, bool, bool)> = None; // (slot_load, bound, slot_is_lhs, yield_is_then)
    for (s, o, s_is_lhs) in [(a, b, true), (b, a, false)] {
        let Inst::Load(c) = &func.insts[s] else { continue };
        for (yc, dc, yield_is_then) in [(ca, cb, true), (cb, ca, false)] {
            if stores_cell(yc, *c) && !stores_cell(dc, *c) {
                if pick.is_some() {
                    return None; // ambiguous
                }
                pick = Some((s, o, s_is_lhs, yield_is_then));
            }
        }
    }
    let (slot_load, bound, slot_is_lhs, yield_is_then) = pick?;
    let Inst::Load(slot) = func.insts[slot_load] else { return None };
    let (y, y_arm, d, d_chain) = if yield_is_then {
        (ca, then_bb, cb, else_chain)
    } else {
        (cb, else_bb, ca, then_chain)
    };
    if y == t || d == t || y == d {
        return None;
    }

    // Normalize to an up-counting continue-comparison with the slot on the LHS.
    let slot_lhs_cmp = if slot_is_lhs {
        cmp
    } else {
        match cmp {
            Cmp::Lt => Cmp::Gt,
            Cmp::Leq => Cmp::Geq,
            Cmp::Gt => Cmp::Lt,
            Cmp::Geq => Cmp::Leq,
        }
    };
    let cont = if yield_is_then {
        slot_lhs_cmp
    } else {
        match slot_lhs_cmp {
            Cmp::Lt => Cmp::Geq,
            Cmp::Leq => Cmp::Gt,
            Cmp::Gt => Cmp::Leq,
            Cmp::Geq => Cmp::Lt,
        }
    };
    let inclusive = match cont {
        Cmp::Lt => false,
        Cmp::Leq => true,
        _ => return None, // downward loop -- the detector only prints up-counting `for`
    };

    // Both paths must converge on the flag-join block J.
    let Term::Goto(jy) = func.blocks[y].term else { return None };
    let Term::Goto(jd) = func.blocks[d].term else { return None };
    let mut y_join_chain = Vec::new();
    let j = chase(func, jy, &mut y_join_chain);
    let mut d_join_chain = Vec::new();
    if chase(func, jd, &mut d_join_chain) != j {
        return None;
    }
    // J: exactly `f = load flag; branch f -> BODY | EXIT`.
    if func.blocks[j].insts.len() != 1 {
        return None;
    }
    let fload = func.blocks[j].insts[0];
    let Inst::Load(flag) = func.insts[fload] else { return None };
    let Term::Branch { cond: jc, then_bb: body, else_bb: exit } = func.blocks[j].term else {
        return None;
    };
    if jc != fload || flag == slot || body == exit {
        return None;
    }

    // Flag is stored as literal `true` on Y and literal `false` on D, once each.
    let flag_const = |blk: Idx<Block>, want: bool| -> bool {
        let mut seen = false;
        for &i in &func.blocks[blk].insts {
            if let Inst::Store { cell, value } = &func.insts[i] {
                if *cell == flag {
                    if seen
                        || !matches!(&func.insts[*value], Inst::Const(Lit::Bool(w)) if *w == want)
                    {
                        return false;
                    }
                    seen = true;
                }
            }
        }
        seen
    };
    if !flag_const(y, true) || !flag_const(d, false) {
        return None;
    }

    // D must be droppable: consts, stores to non-slot cells, or `phantom()`.
    for &i in &func.blocks[d].insts {
        match &func.insts[i] {
            Inst::Const(_) => {}
            Inst::Store { cell, .. } if *cell != slot => {}
            Inst::Call { func: fr, args } if args.is_empty() && fn_name(ws, *fr) == "phantom" => {}
            _ => return None,
        }
    }

    // Y: exactly one slot store, a `slot + 1` increment, no slot read after it.
    let yinsts = &func.blocks[y].insts;
    let mut incr_pos = None;
    for (pos, &i) in yinsts.iter().enumerate() {
        if let Inst::Store { cell, value } = &func.insts[i] {
            if *cell == slot {
                if incr_pos.is_some() {
                    return None;
                }
                let Inst::IntAdd(x, yv) = &func.insts[*value] else { return None };
                let is_slot = |ii: Idx<Inst>| {
                    matches!(&func.insts[ii], Inst::Load(c) if *c == slot)
                };
                let is_one =
                    |ii: Idx<Inst>| matches!(&func.insts[ii], Inst::Const(Lit::Int(1)));
                if !((is_slot(*x) && is_one(*yv)) || (is_slot(*yv) && is_one(*x))) {
                    return None;
                }
                incr_pos = Some(pos);
            }
        }
    }
    let incr_pos = incr_pos?;
    for &i in &yinsts[incr_pos + 1..] {
        if matches!(&func.insts[i], Inst::Load(c) if *c == slot) {
            return None;
        }
    }

    // Preds of the head: back-edges are reachable from BODY; the preheader is
    // the single one that isn't, and must be a plain goto (it falls through
    // into the emitted `for`).
    let mut rb: HashSet<Idx<Block>> = HashSet::default();
    rb.insert(body);
    let mut stack = vec![body];
    while let Some(bk) = stack.pop() {
        targets(&func.blocks[bk].term, |s| {
            if rb.insert(s) {
                stack.push(s);
            }
        });
    }
    let mut back = Vec::new();
    let mut pre = Vec::new();
    for (bid, blk) in func.blocks.iter() {
        let mut hits = false;
        targets(&blk.term, |s| {
            if s == h {
                hits = true;
            }
        });
        if hits {
            if rb.contains(&bid) {
                back.push(bid);
            } else {
                pre.push(bid);
            }
        }
    }
    if pre.len() != 1 || back.is_empty() {
        return None;
    }
    let p = pre[0];
    if func.blocks[p].term != Term::Goto(h) {
        return None;
    }

    // The natural-loop region: everything reaching a back-edge source without
    // crossing the head/test. (Includes Y, D, J, the arm chains, and BODY.)
    let mut preds: HashMap<Idx<Block>, Vec<Idx<Block>>> = HashMap::default();
    for (bid, blk) in func.blocks.iter() {
        targets(&blk.term, |s| preds.entry(s).or_default().push(bid));
    }
    let mut region: HashSet<Idx<Block>> = back.iter().copied().collect();
    let mut stack: Vec<Idx<Block>> = back.clone();
    while let Some(bk) = stack.pop() {
        for &pp in preds.get(&bk).map(|v| v.as_slice()).unwrap_or(&[]) {
            if pp != h && pp != t && region.insert(pp) {
                stack.push(pp);
            }
        }
    }
    let in_loop: HashSet<Idx<Block>> =
        region.iter().copied().chain([h, t]).collect();

    // Slot soundness, fn-wide (all arena blocks, incl. closures/unreachable):
    // exactly one init store outside the loop, no other in-loop store than the
    // increment, loads only in T (the compare) and Y (pre-increment, checked
    // above), never a multi-call dest / map-next out-cell / lambda param.
    let mut init_stores = 0usize;
    for (bid, blk) in func.blocks.iter() {
        let inside = in_loop.contains(&bid);
        for &i in &blk.insts {
            match &func.insts[i] {
                Inst::Store { cell, .. } if *cell == slot => {
                    if inside {
                        if bid != y {
                            return None;
                        }
                    } else {
                        init_stores += 1;
                    }
                }
                Inst::Load(c) if *c == slot => {
                    if !inside || (bid != t && bid != y) {
                        return None;
                    }
                }
                Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. }
                    if dests.contains(&slot) =>
                {
                    return None
                }
                Inst::MapNext { key_cell, val_cell, .. }
                    if *key_cell == slot || *val_cell == slot =>
                {
                    return None
                }
                Inst::Lambda { params, .. } if params.contains(&slot) => return None,
                _ => {}
            }
        }
    }
    if init_stores != 1 {
        return None;
    }

    // Flag hygiene: its only read is J's branch load (D's `false` store dies
    // with D, so any other reader would observe the change).
    for (bid, blk) in func.blocks.iter() {
        for &i in &blk.insts {
            if matches!(&func.insts[i], Inst::Load(c) if *c == flag) && (bid != j || i != fload) {
                return None;
            }
        }
    }
    // Done-value hygiene: cells D stores are never read outside the loop (an
    // outside reader would observe the dropped done-value write).
    for &i in &func.blocks[d].insts {
        if let Inst::Store { cell, .. } = &func.insts[i] {
            for (bid, blk) in func.blocks.iter() {
                if in_loop.contains(&bid) || bid == j {
                    continue;
                }
                for &k in &blk.insts {
                    if matches!(&func.insts[k], Inst::Load(c) if c == cell) {
                        return None;
                    }
                }
            }
        }
    }

    // Bound chain: the T-resident insts computing the bound. `Load`s of cells
    // unwritten in the loop, `Field` projections over them (the `.len` read --
    // hoisting it is the sanctioned snapshot), and consts. An operand defined
    // OUTSIDE T already dominates the loop and needs no move.
    let tset: HashSet<Idx<Inst>> = func.blocks[t].insts.iter().copied().collect();
    if !tset.contains(&slot_load) || !tset.contains(&cond) {
        return None;
    }
    let mut chain_set: HashSet<Idx<Inst>> = HashSet::default();
    let mut work = vec![bound];
    while let Some(v) = work.pop() {
        if !tset.contains(&v) || !chain_set.insert(v) {
            continue;
        }
        match &func.insts[v] {
            Inst::Const(_) => {}
            Inst::Load(c) => {
                if cell_written_in(func, *c, &in_loop) {
                    return None;
                }
            }
            Inst::Field { base, .. } => work.push(*base),
            _ => return None,
        }
    }
    // T must contain nothing but the compare pieces (the detector's header
    // rule: any extra per-iteration inst would be silently hoisted).
    for &i in &func.blocks[t].insts {
        if i != slot_load && i != cond && !chain_set.contains(&i) {
            return None;
        }
    }
    let chain: Vec<Idx<Inst>> = func.blocks[t]
        .insts
        .iter()
        .copied()
        .filter(|i| chain_set.contains(i))
        .collect();

    // Scaffolding to clear once unlinked.
    let mut clear = head_chain;
    clear.push(d);
    clear.extend(d_chain);
    clear.extend(d_join_chain);
    clear.extend(y_join_chain);
    clear.push(j);
    clear.retain(|&bk| bk != t && bk != y && bk != y_arm);

    Some(LoopMatch {
        t,
        p,
        y,
        y_arm,
        body,
        exit,
        slot,
        bound,
        inclusive,
        incr_pos,
        chain,
        back,
        clear,
    })
}

fn rewrite(func: &mut MirFunc, h: Idx<Block>, m: LoopMatch) {
    // Hoist the bound chain into the preheader (snapshot; order preserved).
    let chain_set: HashSet<Idx<Inst>> = m.chain.iter().copied().collect();
    func.blocks[m.t].insts.retain(|i| !chain_set.contains(i));
    func.blocks[m.p].insts.extend(m.chain.iter().copied());

    // T becomes the detector header: `load slot; cmp` -> Branch(yield, exit).
    let sl = func.insts.alloc(Inst::Load(m.slot));
    let ncond = func.insts.alloc(if m.inclusive {
        Inst::IntLeq(sl, m.bound)
    } else {
        Inst::IntLt(sl, m.bound)
    });
    func.blocks[m.t].insts = vec![sl, ncond];
    func.blocks[m.t].term = Term::Branch {
        cond: ncond,
        then_bb: m.y_arm,
        else_bb: m.exit,
    };

    // Y drops the increment (the `for` owns it now) and runs straight into
    // the body; its dead flag store falls to DCE once J is gone.
    func.blocks[m.y].insts.remove(m.incr_pos);
    func.blocks[m.y].term = Term::Goto(m.body);

    // Dedicated 4-inst latch (the exact `latch_slot` shape); every former
    // back-edge -- loop end AND continue -- goes through it.
    let l = func.new_block();
    let ll = func.insts.alloc(Inst::Load(m.slot));
    let lc = func.insts.alloc(Inst::Const(Lit::Int(1)));
    let la = func.insts.alloc(Inst::IntAdd(ll, lc));
    let ls = func.insts.alloc(Inst::Store { cell: m.slot, value: la });
    func.blocks[l].insts = vec![ll, lc, la, ls];
    func.blocks[l].term = Term::Goto(m.t);
    for &bp in &m.back {
        retarget(&mut func.blocks[bp].term, h, l);
    }
    func.blocks[m.p].term = Term::Goto(m.t);

    // Clear the unlinked scaffolding: the detector's fn-wide scans
    // (`slot_sound`, DCE's use counts) see ALL arena blocks, so stale insts in
    // unreachable blocks would keep dead stores alive or veto the match.
    for &bk in &m.clear {
        func.blocks[bk].insts.clear();
        func.blocks[bk].term = Term::Unreachable;
    }
    if h != m.t {
        func.blocks[h].insts.clear();
        func.blocks[h].term = Term::Unreachable;
    }
}
