//! Recover a counted loop from an inlined iterator's test, yield and done arms.
//! Only tagged iterator loops may snapshot their bound. A handwritten while
//! must still read it on each iteration.
//!
//! The rewrite keeps yield-path copies, routes filter rejects to the latch
//! and take_while rejects to the exit, and clears the obsolete flag branches.
//! The latch now advances the cursor, so continue advances it once as before.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;

pub fn recover_counted_loops(ws: &Workspace, func: &mut MirFunc) {
    if func.for_iter_headers.is_empty() {
        return;
    }
    let headers = func.for_iter_headers.clone();
    let mut changed = false;
    // The dominator tree is shared by the headers until a rewrite reshapes
    // the CFG; the next header then recomputes it.
    let mut idom = None;
    for h in headers {
        if let Some(m) = try_match(ws, func, h, &mut idom) {
            rewrite(func, h, m);
            idom = None;
            changed = true;
        }
    }
    if changed {
        crate::dce(func);
    }
}

struct LoopMatch {
    /// Test block: holds the compare + branch (== the head when it has insts).
    t: Idx<Block>,
    /// Unique non-back-edge pred of the head; must end `Goto(head)`.
    p: Idx<Block>,
    y: Idx<Block>,
    y_arm: Idx<Block>,
    exit: Idx<Block>,
    slot: Idx<Cell>,
    bound: Idx<Inst>,
    inclusive: bool,
    incr_pos: usize,
    chain: Vec<Idx<Inst>>,
    back: Vec<Idx<Block>>,
    clear: Vec<Idx<Block>>,
    /// Join rewrites in order from the iterator toward the loop body.
    joins: Vec<(Idx<Block>, Vec<Idx<Inst>>, Idx<Block>)>,
    /// Skip-reject arms `(branch block, raw arm target)`: a filter reject
    /// that re-enters the fetch. Retargeted to the latch (a `continue`).
    skips: Vec<(Idx<Block>, Idx<Block>)>,
    stops: Vec<(Idx<Block>, Idx<Block>)>,
}

struct GuardInfo {
    b: Idx<Block>,
    gload: Idx<Inst>,
    cell: Idx<Cell>,
    done_arm: Idx<Block>,
}

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

fn retarget(term: &mut Term, from: Idx<Block>, to: Idx<Block>) {
    term.remap_targets(&|target| if target == from { to } else { target });
}

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

/// The flag cell a combinator join branches on.
fn branch_flag(func: &MirFunc, b: Idx<Block>) -> Option<Idx<Cell>> {
    let Term::Branch { cond, .. } = func.blocks[b].term else {
        return None;
    };
    let load = match func.insts[cond] {
        Inst::Not(x) => x,
        _ => cond,
    };
    match func.insts[load] {
        Inst::Load(flag) if func.blocks[b].insts.contains(&load) => Some(flag),
        _ => None,
    }
}

fn fn_name(ws: &Workspace, fr: FuncRef) -> &str {
    ws.module(fr.module).functions[fr.func].name.as_str()
}

fn try_match(
    ws: &Workspace,
    func: &MirFunc,
    h: Idx<Block>,
    idom: &mut Option<skull_mir::cfg::Dominators>,
) -> Option<LoopMatch> {
    // A take_while done guard can disappear if false on every retained entry.
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

    let (less, equal, a, b) = match func.insts[cond] {
        Inst::IntLt(a, b) => (true, false, a, b),
        Inst::IntLeq(a, b) => (true, true, a, b),
        Inst::IntGt(a, b) => (false, false, a, b),
        Inst::IntGeq(a, b) => (false, true, a, b),
        _ => return None,
    };

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

    // Swapping operands or taking the false arm reverses the comparison.
    if less != (slot_is_lhs == yield_is_then) {
        return None;
    }
    let inclusive = equal == yield_is_then;

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
    let stores_false = |block: Idx<Block>| {
        func.blocks[block].insts.iter().any(|&id| {
            matches!(func.insts[id], Inst::Store { value, .. }
                if matches!(func.insts[value], Inst::Const(Lit::Bool(false))))
        })
    };
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

    // Each combinator can add one flag join. Follow its true arm through the
    // element copies and use its false arm to find the next join or the exit.
    struct Join {
        j: Idx<Block>,
        drop: Vec<Idx<Inst>>,
        cont: Idx<Block>,
        flag: Idx<Cell>,
    }
    let mut joins: Vec<Join> = Vec::new();
    let mut done_blocks: Vec<Idx<Block>> = vec![d];
    let mut done_chains: Vec<Idx<Block>> = d_join_chain;
    let mut yield_sources: Vec<Vec<Idx<Block>>> = vec![vec![y]];
    let mut live_hops: Vec<Idx<Block>> = y_join_chain;
    let mut skips: Vec<(Idx<Block>, Idx<Block>)> = Vec::new();
    let mut stops: Vec<(Idx<Block>, Idx<Block>)> = Vec::new();
    // Stop arms' done blocks: verified to store the NEXT level's flag `false`
    // once that flag is known (top of the following peel iteration).
    let mut pending_stops: Vec<(Idx<Block>, Idx<Block>, Idx<Block>)> = Vec::new();
    let mut stop_dones: Vec<Idx<Block>> = Vec::new();
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
        // SROA can reuse the flag cell for an element copy after the flag load.
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
        for (sb, sa, sd) in pending_stops.drain(..) {
            if !d_droppable(sd) || !flag_const(sd, flag, false) {
                return None;
            }
            stops.push((sb, sa));
            stop_dones.push(sd);
        }
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

        let mut done_arm_chain = Vec::new();
        let dn = chase(func, done_arm, &mut done_arm_chain);
        let deeper = d_droppable(dn)
            && match func.blocks[dn].term {
                Term::Goto(next) => {
                    let next = chase(func, next, &mut Vec::new());
                    branch_flag(func, next).is_some_and(|f| flag_const(dn, f, false))
                }
                _ => false,
            };
        if !deeper {
            break (cont_arm, done_arm);
        }
        done_blocks.push(dn);
        done_chains.extend(done_arm_chain);

        // A branch on the path to the next join needs one skip or stop arm.
        let Term::Goto(dgoto) = func.blocks[dn].term else {
            return None;
        };
        let mut next_chain = Vec::new();
        let next_j = chase(func, dgoto, &mut next_chain);
        done_chains.extend(next_chain);
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
                    if chase(func, g, &mut h2) == next_j && d_droppable(x) && stores_false(x) {
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

    // Dominance distinguishes a latch from a preheader inside an outer loop.
    let idom = idom.get_or_insert_with(|| skull_mir::cfg::dominators(func, func.entry()));
    let mut back = Vec::new();
    let mut pre = Vec::new();
    for (bid, blk) in func.blocks.iter() {
        let mut hits = false;
        successors(&blk.term, |s| {
            if s == h {
                hits = true;
            }
        });
        if hits {
            if skull_mir::cfg::dominates(idom, h, bid) {
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
        successors(&blk.term, |s| preds.entry(s).or_default().push(bid));
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

    // Other readers could observe the change from pre-body to latch increment.
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

    // Guard stores must start false. True writes only occur on paths
    // removed by the rewrite, which exit the loop.
    let mut guard_clear: Vec<Idx<Block>> = Vec::new();
    for g in &guards {
        let mut hops = Vec::new();
        let dg = chase(func, g.done_arm, &mut hops);
        let Term::Goto(gg) = func.blocks[dg].term else {
            return None;
        };
        let mut h2 = Vec::new();
        let tgt = chase(func, gg, &mut h2);
        let jn = joins.iter().find(|jn| jn.j == tgt)?;
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

    // Flag cells may be shared by levels, but reads must stay on the live yield path.
    let live_set: HashSet<Idx<Block>> = joins
        .iter()
        .map(|jn| jn.j)
        .chain(yield_sources.iter().flatten().copied())
        .chain(live_hops.iter().copied())
        .chain([t, y, y_arm])
        .collect();
    let join_set: HashSet<Idx<Block>> = joins.iter().map(|jn| jn.j).collect();
    let flags: HashSet<Idx<Cell>> = joins.iter().map(|jn| jn.flag).collect();
    let done_writes: HashSet<Idx<Cell>> = g_store_blocks
        .iter()
        .flat_map(|&block| &func.blocks[block].insts)
        .filter_map(|&id| match func.insts[id] {
            Inst::Store { cell, .. } => Some(cell),
            _ => None,
        })
        .collect();
    for (bid, block) in func.blocks.iter() {
        let outside = !in_loop.contains(&bid) && !join_set.contains(&bid);
        let on_yield_path = live_set.contains(&bid);
        for &id in &block.insts {
            if let Inst::Load(cell) = func.insts[id]
                && ((outside && done_writes.contains(&cell))
                    || (!on_yield_path
                        && flags.contains(&cell)
                        && !guards.iter().any(|g| g.gload == id)))
            {
                return None;
            }
        }
    }

    // Hoist only a bound derived from values whose cells stay stable in the loop.
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
    let chain_set: HashSet<Idx<Inst>> = m.chain.iter().copied().collect();
    func.blocks[m.t].insts.retain(|i| !chain_set.contains(i));
    func.blocks[m.p].insts.extend(m.chain.iter().copied());

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

    func.blocks[m.y].insts.remove(m.incr_pos);
    for (jb, drop, cont) in &m.joins {
        func.blocks[*jb].insts.retain(|i| !drop.contains(i));
        func.blocks[*jb].term = Term::Goto(*cont);
    }

    // Codegen recognizes this latch as the numeric-for increment.
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

    // The counted-loop detector scans even unreachable blocks for cursor uses.
    for &bk in &m.clear {
        func.blocks[bk].insts.clear();
        func.blocks[bk].term = Term::Unreachable;
    }
    if h != m.t {
        func.blocks[h].insts.clear();
        func.blocks[h].term = Term::Unreachable;
    }
}
