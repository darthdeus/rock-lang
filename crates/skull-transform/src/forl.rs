//! FORL recovery: rewrite the desugared `for x in <user iter>` loop (after
//! inlining + SROA) into the exact goto-loop shape `skull-codegen-lua`'s
//! `CountedLoop` detector recognizes, so it prints as a native Lua numeric
//! `for` (LuaJIT FORL bytecode: hoisted bounds check, counted back-edge,
//! int<->double transition tolerance). The goto-LOOP form of the same loop
//! measures 1.8x slower on a type-stable 2M-element sum.
//!
//! Fires ONLY on loops tagged by `lower_for_user_iter`
//! (`MirFunc::for_iter_headers`). The one observable change is bound SNAPSHOT:
//! the per-iteration bound read (e.g. the inlined `it.vec.len`) moves to the
//! preheader and is evaluated once per loop. That diverges only when the body
//! mutates the collection being iterated, which is recorded UB. The sanction is exactly why the
//! pass is tag-gated
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
//! Skip-loop layers (filter/take_while) extend the join cascade with three
//! shapes, turning the chain into a flat `for + skip-branch`:
//! - **skip edge** (filter reject): a yield-path branch whose reject arm
//!   chases back to the fetch head (T or a guard). Rewritten to target the
//!   latch -- a `continue`.
//! - **stop edge** (take_while reject): a yield-path branch whose reject arm
//!   chases to a droppable done block storing the NEXT level's flag `false`.
//!   Rewritten to target EXIT -- a `break`.
//! - **guard** (take_while `__done`): a head-chain block
//!   `g = load cell; branch g -> done | fetch`. `g` is provably false at
//!   every entry (init-false store outside the loop; `true` stores only in
//!   done/stop blocks, which exit the rewritten loop and are cleared; no
//!   other reader), so the guard drops with the head chain.
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
/// codegen `CountedLoop` shape. Run after `inline_functions_with_plans` + `cleanup` (the
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
    /// Scaffolding to clear: head chain + every done-path block/chain.
    clear: Vec<Idx<Block>>,
    /// The join cascade, innermost first: `(join block, insts to drop -- the
    /// flag load and optional `not` --, yield-continuation target)`. Each is
    /// rewritten into a pass-through (elem copies kept, `Goto(cont)`); the
    /// LAST one's cont is the loop body.
    joins: Vec<(Idx<Block>, Vec<Idx<Inst>>, Idx<Block>)>,
    /// Skip-reject arms `(branch block, raw arm target)`: a filter reject
    /// that re-enters the fetch. Retargeted to the latch (a `continue`).
    skips: Vec<(Idx<Block>, Idx<Block>)>,
    /// Stop-reject arms `(branch block, raw arm target)`: a take_while
    /// reject whose done path exits the loop. Retargeted to EXIT (a `break`).
    stops: Vec<(Idx<Block>, Idx<Block>)>,
}

/// A take_while `__done` guard on the head chain.
struct GuardInfo {
    b: Idx<Block>,
    gload: Idx<Inst>,
    cell: Idx<Cell>,
    done_arm: Idx<Block>,
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
        Term::Branch {
            then_bb, else_bb, ..
        } => {
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
        Term::Branch {
            then_bb, else_bb, ..
        } => {
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
                    return true;
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } if *key_cell == c || *val_cell == c => return true,
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
    // Head -> test block through empty gotos, allowing take_while `__done`
    // GUARD blocks on the way: `g = load cell; branch g -> done | fetch`
    // (possibly through a `not`). The continue arm must be the g == false arm
    // -- the guard is dropped on the proof that g is false at every entry,
    // verified fn-wide after the peel.
    let mut head_chain = Vec::new();
    let mut guards: Vec<GuardInfo> = Vec::new();
    let mut cur = h;
    let (t, cond, then_bb, else_bb) = loop {
        let c = chase(func, cur, &mut head_chain);
        let Term::Branch {
            cond,
            then_bb,
            else_bb,
        } = func.blocks[c].term
        else {
            return None;
        };
        if matches!(
            &func.insts[cond],
            Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
        ) {
            break (c, cond, then_bb, else_bb);
        }
        if guards.len() >= 4 || then_bb == else_bb {
            return None;
        }
        let cinsts = &func.blocks[c].insts;
        let (gload, cont_arm) = match &func.insts[cond] {
            Inst::Load(_) if cinsts.len() == 1 && cinsts[0] == cond => (cond, else_bb),
            Inst::Not(x)
                if cinsts.len() == 2
                    && cinsts.contains(&cond)
                    && cinsts.contains(x)
                    && matches!(func.insts[*x], Inst::Load(_)) =>
            {
                (*x, then_bb)
            }
            _ => return None,
        };
        let done_arm = if cont_arm == then_bb {
            else_bb
        } else {
            then_bb
        };
        let Inst::Load(gcell) = func.insts[gload] else {
            return None;
        };
        guards.push(GuardInfo {
            b: c,
            gload,
            cell: gcell,
            done_arm,
        });
        head_chain.push(c);
        cur = cont_arm;
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
        let Inst::Load(c) = &func.insts[s] else {
            continue;
        };
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
    let Inst::Load(slot) = func.insts[slot_load] else {
        return None;
    };
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
    let Term::Goto(jy) = func.blocks[y].term else {
        return None;
    };
    let Term::Goto(jd) = func.blocks[d].term else {
        return None;
    };
    let mut y_join_chain = Vec::new();
    let j = chase(func, jy, &mut y_join_chain);
    let mut d_join_chain = Vec::new();
    if chase(func, jd, &mut d_join_chain) != j {
        return None;
    }

    // D must be droppable: consts, stores to non-slot cells, or `phantom()`.
    let d_droppable = |blk: Idx<Block>| -> bool {
        func.blocks[blk]
            .insts
            .iter()
            .all(|&i| match &func.insts[i] {
                Inst::Const(_) => true,
                Inst::Store { cell, .. } if *cell != slot => true,
                Inst::Call { func: fr, args }
                    if args.is_empty() && fn_name(ws, *fr) == "phantom" =>
                {
                    true
                }
                _ => false,
            })
    };
    // Flag is stored as literal `want` in `blk`, exactly once.
    let flag_const = |blk: Idx<Block>, flag: Idx<Cell>, want: bool| -> bool {
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

    // Peel the JOIN CASCADE. A plain user iterator has ONE flag join
    // (`J: f = load flag; branch f -> BODY | EXIT`). An inlined combinator
    // chain (MapIter over VecIter, ..) has one PER LAYER: each inner `next`'s
    // ok-flag joins into the enclosing `next`'s destructure, whose `if !ok`
    // branches to the NEXT done-propagation block (false side) or continues
    // transforming the element (true side) toward the next join. Peel level by
    // level: the false side chasing to a droppable done block that stores the
    // next flag `false` means one more level; anything else is the loop EXIT
    // and the true side is the BODY. Each join is later rewritten into a
    // pass-through (its elem-copy insts kept, flag load dropped), so the
    // yield path runs straight T -> Y -> J1' -> .. -> Jn' -> BODY.
    struct Join {
        j: Idx<Block>,
        /// Insts to drop from J at rewrite (the flag load + optional `not`).
        drop: Vec<Idx<Inst>>,
        /// The yield-continuation arm J branches to (raw, pre-chase).
        cont: Idx<Block>,
        flag: Idx<Cell>,
    }
    let mut joins: Vec<Join> = Vec::new();
    let mut done_blocks: Vec<Idx<Block>> = vec![d];
    let mut done_chains: Vec<Idx<Block>> = d_join_chain;
    // Per level: the yield-side blocks that must store `flag = true`.
    let mut yield_sources: Vec<Vec<Idx<Block>>> = vec![vec![y]];
    // Yield-path hop blocks (live after the rewrite; part of the flag-read
    // live set below).
    let mut live_hops: Vec<Idx<Block>> = y_join_chain;
    let mut skips: Vec<(Idx<Block>, Idx<Block>)> = Vec::new();
    let mut stops: Vec<(Idx<Block>, Idx<Block>)> = Vec::new();
    // Stop arms' done blocks: verified to store the NEXT level's flag `false`
    // once that flag is known (top of the following peel iteration).
    let mut pending_stops: Vec<(Idx<Block>, Idx<Block>, Idx<Block>)> = Vec::new();
    let mut stop_dones: Vec<Idx<Block>> = Vec::new();
    // Reject-arm hop chains + stop done chains: dead after the retarget.
    let mut reject_chains: Vec<Idx<Block>> = Vec::new();
    let mut cur_j = j;
    let (body, exit) = loop {
        if joins.len() > 8 {
            return None; // cap cascade depth
        }
        let cur_d = *done_blocks.last().unwrap();
        if !d_droppable(cur_d) {
            return None;
        }
        // J's insts: the flag load feeding the branch (directly or through a
        // single `not`), plus elem copies (loads / stores to non-slot cells /
        // consts) that stay on the rewritten pass-through. A store to the
        // flag cell itself is an elem copy REUSING the cell (SROA coalesces a
        // level's ok flag onto the for-desugar's dest cells) -- allowed only
        // AFTER the flag load, so the branch still tests the entry value.
        let Term::Branch {
            cond: jc,
            then_bb,
            else_bb,
        } = func.blocks[cur_j].term
        else {
            return None;
        };
        let jinsts: HashSet<Idx<Inst>> = func.blocks[cur_j].insts.iter().copied().collect();
        let (fload, drop, flag_is_then) = match &func.insts[jc] {
            Inst::Load(_) => (jc, vec![jc], true),
            Inst::Not(x) => {
                if !jinsts.contains(&jc) {
                    return None;
                }
                (*x, vec![*x, jc], false)
            }
            _ => return None,
        };
        if !jinsts.contains(&fload) {
            return None;
        }
        let Inst::Load(flag) = func.insts[fload] else {
            return None;
        };
        if flag == slot || then_bb == else_bb {
            return None;
        }
        let mut seen_fload = false;
        for &i in &func.blocks[cur_j].insts {
            if i == fload {
                seen_fload = true;
                continue;
            }
            if i == jc {
                continue;
            }
            match &func.insts[i] {
                Inst::Const(_) | Inst::Load(_) => {}
                Inst::Store { cell, .. } if *cell == flag => {
                    if !seen_fload {
                        return None;
                    }
                }
                Inst::Store { cell, .. } if *cell != slot => {}
                _ => return None,
            }
        }
        // Stop arms recorded on the walk INTO this join reject at this
        // level: their done block must store this flag false.
        for (sb, sa, sd) in pending_stops.drain(..) {
            if !d_droppable(sd) || !flag_const(sd, flag, false) {
                return None;
            }
            stops.push((sb, sa));
            stop_dones.push(sd);
        }
        // `flag == true` is the yield side (stored true by this level's yield
        // sources, false by its done block).
        let (cont_arm, done_arm) = if flag_is_then {
            (then_bb, else_bb)
        } else {
            (else_bb, then_bb)
        };
        if !flag_const(cur_d, flag, false) {
            return None;
        }
        if !yield_sources
            .last()
            .unwrap()
            .iter()
            .any(|&yb| flag_const(yb, flag, true))
        {
            return None;
        }
        joins.push(Join {
            j: cur_j,
            drop,
            cont: cont_arm,
            flag,
        });

        // Probe the done arm: another done-propagation block means one more
        // level; anything else is the real EXIT.
        let mut done_arm_chain = Vec::new();
        let dn = chase(func, done_arm, &mut done_arm_chain);
        let deeper = d_droppable(dn)
            && matches!(func.blocks[dn].term, Term::Goto(_))
            && func.blocks[dn].insts.iter().any(|&i| {
                matches!(&func.insts[i], Inst::Store { value, .. }
                    if matches!(&func.insts[*value], Inst::Const(Lit::Bool(false))))
            });
        if !deeper {
            break (cont_arm, done_arm);
        }
        done_blocks.push(dn);
        done_chains.extend(done_arm_chain);

        // Walk the yield side to the next join (the done side's convergence).
        // The path is goto-linked, insts allowed; a BRANCH block on it must
        // have exactly one reject arm -- a skip (chases to the fetch head: T
        // or a guard) or a stop (chases to a droppable done block converging
        // at the next join) -- and the walk continues on the other arm.
        let Term::Goto(dgoto) = func.blocks[dn].term else {
            return None;
        };
        let mut next_chain = Vec::new();
        let next_j = chase(func, dgoto, &mut next_chain);
        done_chains.extend(next_chain);
        // `Some((target, is_skip))` = reject arm; `None` = not one.
        let classify =
            |arm: Idx<Block>, chains: &mut Vec<Idx<Block>>| -> Option<(Idx<Block>, bool)> {
                let mut hops = Vec::new();
                let x = chase(func, arm, &mut hops);
                if x == t || guards.iter().any(|g| g.b == x) {
                    chains.extend(hops);
                    return Some((x, true));
                }
                if let Term::Goto(g) = func.blocks[x].term {
                    let mut h2 = Vec::new();
                    if chase(func, g, &mut h2) == next_j
                        && d_droppable(x)
                        && func.blocks[x].insts.iter().any(|&i| {
                            matches!(&func.insts[i], Inst::Store { value, .. }
                            if matches!(&func.insts[*value], Inst::Const(Lit::Bool(false))))
                        })
                    {
                        chains.extend(hops);
                        chains.push(x);
                        chains.extend(h2);
                        return Some((x, false));
                    }
                }
                None
            };
        let mut ysrc = Vec::new();
        let mut yb = cont_arm;
        let mut steps = 0;
        loop {
            // Yield-side hop blocks stay LIVE (the rewritten pass-through path
            // runs through them) -- never into the clear list.
            let mut hops = Vec::new();
            let c = chase(func, yb, &mut hops);
            live_hops.extend(hops);
            if c == next_j {
                break;
            }
            ysrc.push(c);
            match func.blocks[c].term {
                Term::Goto(nxt) => yb = nxt,
                Term::Branch {
                    then_bb: tb,
                    else_bb: eb,
                    ..
                } => {
                    if tb == eb {
                        return None;
                    }
                    let ct = classify(tb, &mut reject_chains);
                    let ce = classify(eb, &mut reject_chains);
                    let (arm_raw, rej, cont2) = match (ct, ce) {
                        (Some(r), None) => (tb, r, eb),
                        (None, Some(r)) => (eb, r, tb),
                        _ => return None,
                    };
                    let (target, is_skip) = rej;
                    if is_skip {
                        skips.push((c, arm_raw));
                    } else {
                        pending_stops.push((c, arm_raw, target));
                    }
                    yb = cont2;
                }
                _ => return None,
            }
            steps += 1;
            if steps > 16 {
                return None;
            }
        }
        yield_sources.push(ysrc);
        cur_j = next_j;
    };
    if body == exit || !pending_stops.is_empty() {
        return None;
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
                let Inst::IntAdd(x, yv) = &func.insts[*value] else {
                    return None;
                };
                let is_slot =
                    |ii: Idx<Inst>| matches!(&func.insts[ii], Inst::Load(c) if *c == slot);
                let is_one = |ii: Idx<Inst>| matches!(&func.insts[ii], Inst::Const(Lit::Int(1)));
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

    // Preds of the head: a back edge is a pred the head DOMINATES; the
    // preheader is the single one it does not, and must be a plain goto (it
    // falls through into the emitted `for`). Forward reachability from BODY is
    // the wrong split: inside an enclosing loop the preheader is reachable
    // from the body through the outer back edge, so every nested iterator
    // loop read as having two preheaders and stayed a goto loop. A pred the
    // entry cannot reach has no dominator entry and counts as a preheader,
    // which bails the same way it always did.
    let idom = skull_mir::cfg::dominators(func, func.entry());
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
            if skull_mir::cfg::dominates(&idom, h, bid) {
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
    let in_loop: HashSet<Idx<Block>> = region.iter().copied().chain([h, t]).collect();

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
                    return None;
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } if *key_cell == slot || *val_cell == slot => return None,
                Inst::Lambda { params, .. } if params.contains(&slot) => return None,
                _ => {}
            }
        }
    }
    if init_stores != 1 {
        return None;
    }

    // Guard verification (post-peel, in_loop known). Each guard's done arm
    // must chase to a droppable block storing some peeled join's flag `false`
    // and converging at that join (structurally a done block for that level);
    // it joins the clear list. The guard cell `g` must be provably false at
    // every loop entry: all out-of-loop stores are `false` (>=1), all in-loop
    // stores are `true` AND sit in blocks whose control exits the rewritten
    // loop (done/stop/guard-done blocks -- all cleared), and the guard load
    // is g's only read fn-wide.
    let mut guard_clear: Vec<Idx<Block>> = Vec::new();
    for g in &guards {
        let mut hops = Vec::new();
        let dg = chase(func, g.done_arm, &mut hops);
        let Term::Goto(gg) = func.blocks[dg].term else {
            return None;
        };
        let mut h2 = Vec::new();
        let tgt = chase(func, gg, &mut h2);
        let Some(jn) = joins.iter().find(|jn| jn.j == tgt) else {
            return None;
        };
        if !d_droppable(dg) || !flag_const(dg, jn.flag, false) {
            return None;
        }
        guard_clear.extend(hops);
        guard_clear.push(dg);
        guard_clear.extend(h2);
    }
    let g_store_blocks: HashSet<Idx<Block>> = done_blocks
        .iter()
        .chain(stop_dones.iter())
        .chain(guard_clear.iter())
        .copied()
        .collect();
    for g in &guards {
        if g.cell == slot {
            return None;
        }
        let mut init_false = 0usize;
        for (bid, blk) in func.blocks.iter() {
            for &i in &blk.insts {
                match &func.insts[i] {
                    Inst::Store { cell, value } if *cell == g.cell => {
                        if in_loop.contains(&bid) {
                            if !matches!(&func.insts[*value], Inst::Const(Lit::Bool(true)))
                                || !g_store_blocks.contains(&bid)
                            {
                                return None;
                            }
                        } else {
                            if !matches!(&func.insts[*value], Inst::Const(Lit::Bool(false))) {
                                return None;
                            }
                            init_false += 1;
                        }
                    }
                    Inst::Load(c) if *c == g.cell => {
                        if i != g.gload {
                            return None;
                        }
                    }
                    Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. }
                        if dests.contains(&g.cell) =>
                    {
                        return None;
                    }
                    Inst::MapNext {
                        key_cell, val_cell, ..
                    } if *key_cell == g.cell || *val_cell == g.cell => return None,
                    Inst::Lambda { params, .. } if params.contains(&g.cell) => return None,
                    _ => {}
                }
            }
        }
        if init_false == 0 {
            return None;
        }
    }

    // Flag hygiene, per level: every read of a level flag must sit on the
    // LIVE yield path (T, Y, the joins, yield-source blocks and their hops)
    // -- there no store feeding it is dropped, so it sees identical values
    // pre/post rewrite. SROA reuses one cell as several levels' flags and as
    // an elem temp (the for-desugar dest cells), so "only the join's own
    // fload" is too strict; a read anywhere else (body, after the loop, a
    // closure) could observe a dropped done-path store.
    let live_set: HashSet<Idx<Block>> = joins
        .iter()
        .map(|jn| jn.j)
        .chain(yield_sources.iter().flatten().copied())
        .chain(live_hops.iter().copied())
        .chain([t, y, y_arm])
        .collect();
    for jn in &joins {
        for (bid, blk) in func.blocks.iter() {
            for &i in &blk.insts {
                if matches!(&func.insts[i], Inst::Load(c) if *c == jn.flag)
                    && !live_set.contains(&bid)
                    && !guards.iter().any(|g| g.gload == i)
                {
                    return None;
                }
            }
        }
    }
    // Done-value hygiene, per level: cells a done/stop/guard-done block
    // stores are never read outside the loop (an outside reader would
    // observe the dropped write).
    let join_set: HashSet<Idx<Block>> = joins.iter().map(|jn| jn.j).collect();
    for &dk in done_blocks
        .iter()
        .chain(stop_dones.iter())
        .chain(guard_clear.iter())
    {
        for &i in &func.blocks[dk].insts {
            if let Inst::Store { cell, .. } = &func.insts[i] {
                for (bid, blk) in func.blocks.iter() {
                    if in_loop.contains(&bid) || join_set.contains(&bid) {
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

    // Scaffolding to clear once unlinked: the head chain (guards included),
    // every done-path block (stop and guard done blocks too), and the
    // reject-arm hop chains. The joins are NOT cleared -- they rewrite into
    // yield pass-throughs -- and neither are the Y->J hop chains or
    // yield-walk hops (live on the yield path).
    let mut clear = head_chain;
    clear.extend(done_blocks);
    clear.extend(d_chain);
    clear.extend(done_chains);
    clear.extend(stop_dones);
    clear.extend(reject_chains);
    clear.extend(guard_clear);
    clear.retain(|&bk| {
        bk != t && bk != y && bk != y_arm && !join_set.contains(&bk) && !live_set.contains(&bk)
    });

    let join_rewrites: Vec<(Idx<Block>, Vec<Idx<Inst>>, Idx<Block>)> = joins
        .iter()
        .map(|jn| (jn.j, jn.drop.clone(), jn.cont))
        .collect();

    Some(LoopMatch {
        t,
        p,
        y,
        y_arm,
        exit,
        slot,
        bound,
        inclusive,
        incr_pos,
        chain,
        back,
        clear,
        joins: join_rewrites,
        skips,
        stops,
    })
}

fn rewrite(func: &mut MirFunc, h: Idx<Block>, m: LoopMatch) {
    // Hoist the bound chain into the preheader (snapshot; order preserved).
    let chain_set: HashSet<Idx<Inst>> = m.chain.iter().copied().collect();
    func.blocks[m.t].insts.retain(|i| !chain_set.contains(i));
    func.blocks[m.p].insts.extend(m.chain.iter().copied());

    // T becomes the detector header: `load slot; cmp` -> Branch(yield, exit).
    let sl = func.alloc_inst(Inst::Load(m.slot));
    let ncond = func.alloc_inst(if m.inclusive {
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

    // Y drops the increment (the `for` owns it now) and keeps flowing into
    // the join cascade; each join becomes a pass-through -- flag load (and
    // `not`) dropped, elem copies kept, straight `Goto` to its yield arm (the
    // last one's arm IS the body). The now-dead flag stores fall to DCE.
    func.blocks[m.y].insts.remove(m.incr_pos);
    for (jb, drop, cont) in &m.joins {
        func.blocks[*jb].insts.retain(|i| !drop.contains(i));
        func.blocks[*jb].term = Term::Goto(*cont);
    }

    // Dedicated 4-inst latch (the exact `latch_slot` shape); every former
    // back-edge -- loop end AND continue -- goes through it. A filter's skip
    // edge becomes a `continue` (through the latch: the element was fetched,
    // so the slot advances); a take_while's stop edge becomes a `break`
    // (straight to EXIT -- its done-propagation path is cleared).
    let l = func.new_block();
    let ll = func.alloc_inst(Inst::Load(m.slot));
    let lc = func.alloc_inst(Inst::Const(Lit::Int(1)));
    let la = func.alloc_inst(Inst::IntAdd(ll, lc));
    let ls = func.alloc_inst(Inst::Store {
        cell: m.slot,
        value: la,
    });
    func.blocks[l].insts = vec![ll, lc, la, ls];
    func.blocks[l].term = Term::Goto(m.t);
    for &bp in &m.back {
        retarget(&mut func.blocks[bp].term, h, l);
    }
    for &(sb, raw) in &m.skips {
        retarget(&mut func.blocks[sb].term, raw, l);
    }
    for &(sb, raw) in &m.stops {
        retarget(&mut func.blocks[sb].term, raw, m.exit);
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
