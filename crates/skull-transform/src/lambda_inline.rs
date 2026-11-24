//! Move a lambda's body into its only call when the call sits in the scope that
//! creates the lambda. Inlined iterator combinators reduce `xs.iter().map(f)` to
//! exactly this shape, so the chain's closures stop allocating environments and
//! stop dispatching through code slots.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;

use crate::visit::{raw, top_level_blocks_from};

struct Candidate {
    lambda: Idx<Inst>,
    region: usize,
    body: usize,
}

struct Regions {
    /// Region 0 is the function's own CFG; each lambda body is one more.
    of_block: Vec<Option<usize>>,
    blocks: Vec<Vec<Idx<Block>>>,
    lambdas: Vec<Candidate>,
}

fn regions(mf: &MirFunc) -> Regions {
    let mut r = Regions {
        of_block: vec![None; mf.blocks.len()],
        blocks: Vec::new(),
        lambdas: Vec::new(),
    };
    let mut work = vec![(mf.entry, None)];
    while let Some((entry, lambda)) = work.pop() {
        let region = r.blocks.len();
        let blocks = top_level_blocks_from(mf, entry);
        for &b in &blocks {
            r.of_block[raw(b)] = Some(region);
            for &id in &mf.blocks[b].insts {
                if let Inst::Lambda { entry, .. } = mf.insts[id] {
                    work.push((entry, Some((id, region))));
                }
            }
        }
        if let Some((lambda, creator)) = lambda {
            r.lambdas.push(Candidate {
                lambda,
                region: creator,
                body: region,
            });
        }
        r.blocks.push(blocks);
    }
    r
}

fn callee_and_args(inst: &Inst) -> Option<(Idx<Inst>, &[Idx<Inst>])> {
    match inst {
        Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
            Some((*callee, args))
        }
        _ => None,
    }
}

/// Every lambda used exactly once, as the callee of a call in its creating
/// region, mapped to that call's position.
fn single_calls(mf: &MirFunc, r: &Regions) -> HashMap<Idx<Inst>, (Idx<Block>, usize)> {
    let lambdas: HashSet<Idx<Inst>> = r.lambdas.iter().map(|c| c.lambda).collect();
    let mut uses: HashMap<Idx<Inst>, usize> = HashMap::default();
    let mut calls: HashMap<Idx<Inst>, (Idx<Block>, usize)> = HashMap::default();
    let mut count = |v: Idx<Inst>| {
        if lambdas.contains(&v) {
            *uses.entry(v).or_default() += 1;
        }
    };
    for blocks in &r.blocks {
        for &b in blocks {
            for (pos, &id) in mf.blocks[b].insts.iter().enumerate() {
                let inst = &mf.insts[id];
                inst.uses_each(&mut count);
                if let Some((callee, _)) = callee_and_args(inst)
                    && lambdas.contains(&callee)
                {
                    calls.insert(callee, (b, pos));
                }
            }
            mf.blocks[b].term.uses_each(&mut count);
        }
    }
    calls.retain(|lambda, _| uses.get(lambda) == Some(&1));
    calls
}

/// A by-value capture snapshots its cell when the lambda is created. Inlining
/// reads the cell at the call instead, so a write the call can see without the
/// lambda being created again must block the move.
fn capture_may_change(
    mf: &MirFunc,
    region: &[Idx<Block>],
    captured: &HashSet<Idx<Cell>>,
    lambda: Idx<Inst>,
    call: Idx<Inst>,
) -> bool {
    let mut stack: Vec<(Idx<Block>, usize)> = Vec::new();
    for &b in region {
        for (pos, &id) in mf.blocks[b].insts.iter().enumerate() {
            let mut writes = false;
            mf.insts[id].cell_writes_each(&mut |c| writes |= captured.contains(&c));
            if writes {
                stack.push((b, pos + 1));
            }
        }
    }
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    while let Some((b, from)) = stack.pop() {
        let block = &mf.blocks[b];
        let mut killed = false;
        for &id in &block.insts[from..] {
            if id == call {
                return true;
            }
            if id == lambda {
                killed = true;
                break;
            }
        }
        if !killed {
            successors(&block.term, |s| {
                if seen.insert(s) {
                    stack.push((s, 0));
                }
            });
        }
    }
    false
}

fn movable(mf: &MirFunc, r: &Regions, c: &Candidate, call: Idx<Inst>) -> bool {
    let Inst::Lambda { params, .. } = &mf.insts[c.lambda] else {
        return false;
    };
    let Some((_, args)) = callee_and_args(&mf.insts[call]) else {
        return false;
    };
    if args.len() != params.len()
        || mf.call_abi_plans.contains_key(&call)
        || mf.flat_call_lanes.contains_key(&call)
        || mf.call_rep_plans.get(&call)
            != mf.lambda_abis.get(&c.lambda).map(|abi| &abi.representation)
    {
        return false;
    }
    let dests = match &mf.insts[call] {
        Inst::CallValueMulti { dests, .. } => dests.len(),
        _ => 1,
    };
    let body = &r.blocks[c.body];
    let mut cells: HashSet<Idx<Cell>> = HashSet::default();
    for &b in body {
        for &id in &mf.blocks[b].insts {
            if matches!(mf.insts[id], Inst::Lambda { .. }) {
                return false;
            }
            mf.insts[id].cells_each(&mut |cell| {
                cells.insert(cell);
            });
        }
        match &mf.blocks[b].term {
            Term::Yield { .. } => return false,
            Term::Return(values) if values.len() > dests => return false,
            _ => {}
        }
    }
    let mut captured: HashSet<Idx<Cell>> = HashSet::default();
    for (region, blocks) in r.blocks.iter().enumerate() {
        if region == c.body {
            continue;
        }
        for &b in blocks {
            for &id in &mf.blocks[b].insts {
                if id == c.lambda {
                    continue;
                }
                mf.insts[id].cells_each(&mut |cell| {
                    if cells.contains(&cell) {
                        captured.insert(cell);
                    }
                });
            }
        }
    }
    // A write through a `ref` alias is invisible to the reachability check.
    if captured.iter().any(|&cell| mf.cells[cell].borrowed) {
        return false;
    }
    captured.is_empty() || !capture_may_change(mf, &r.blocks[c.region], &captured, c.lambda, call)
}

fn move_body(mf: &mut MirFunc, body: &[Idx<Block>], lambda: Idx<Inst>, site: (Idx<Block>, usize)) {
    let Inst::Lambda { params, entry } =
        std::mem::replace(&mut mf.insts[lambda], Inst::Const(Lit::Nil))
    else {
        unreachable!("candidate is a lambda")
    };
    mf.lambda_abis.remove(&lambda);
    mf.code_value_identities.remove(&lambda);

    let (block, pos) = site;
    let call = mf.blocks[block].insts[pos];
    let (args, dests) = match &mf.insts[call] {
        Inst::CallValue { args, .. } => (args.clone(), None),
        Inst::CallValueMulti { args, dests, .. } => (args.clone(), Some(dests.clone())),
        _ => unreachable!("site is a value call"),
    };
    let after = mf.blocks[block].insts.split_off(pos + 1);
    mf.blocks[block].insts.pop();
    let cont = mf.new_block();
    mf.blocks[cont].insts = after;
    mf.blocks[cont].term = std::mem::replace(&mut mf.blocks[block].term, Term::Goto(entry));
    let dests = match dests {
        Some(dests) => {
            mf.insts[call] = Inst::Const(Lit::Nil);
            dests
        }
        None => {
            let result = mf.new_cell(None);
            mf.insts[call] = Inst::Load(result);
            mf.blocks[cont].insts.insert(0, call);
            vec![result]
        }
    };
    mf.call_rep_plans.remove(&call);
    for (param, arg) in params.into_iter().zip(args) {
        let store = mf.alloc_inst(Inst::Store {
            cell: param,
            value: arg,
        });
        mf.blocks[block].insts.push(store);
    }
    for &b in body {
        let Term::Return(values) = &mf.blocks[b].term else {
            continue;
        };
        let mut values = values.clone();
        if values.is_empty() && !dests.is_empty() {
            let nil = mf.alloc_inst(Inst::Const(Lit::Nil));
            mf.blocks[b].insts.push(nil);
            values.push(nil);
        }
        for (cell, value) in dests.clone().into_iter().zip(values) {
            let store = mf.alloc_inst(Inst::Store { cell, value });
            mf.blocks[b].insts.push(store);
        }
        mf.blocks[b].term = Term::Goto(cont);
    }
}

/// Returns whether any body moved.
pub(crate) fn inline_lambda_calls(mf: &mut MirFunc) -> bool {
    if skull_mir::pass_disabled("lambda") {
        return false;
    }
    if !mf
        .insts
        .iter()
        .any(|(_, i)| matches!(i, Inst::Lambda { .. }))
    {
        return false;
    }
    let r = regions(mf);
    let calls = single_calls(mf, &r);
    let mut moved: Vec<(usize, Idx<Inst>, (Idx<Block>, usize))> = Vec::new();
    for c in &r.lambdas {
        let Some(&site) = calls.get(&c.lambda) else {
            continue;
        };
        if r.of_block[raw(site.0)] != Some(c.region) {
            continue;
        }
        let call = mf.blocks[site.0].insts[site.1];
        if movable(mf, &r, c, call) {
            moved.push((c.body, c.lambda, site));
        }
    }
    let any = !moved.is_empty();
    // Splitting a block shifts the positions after the call, so move the
    // later calls in a block first.
    moved.sort_by_key(|&(_, _, (b, pos))| (raw(b), std::cmp::Reverse(pos)));
    for (body, lambda, site) in moved {
        move_body(mf, &r.blocks[body], lambda, site);
    }
    any
}
