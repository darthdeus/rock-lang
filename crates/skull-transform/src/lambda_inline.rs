//! Move a lambda's body into its calls when all calls sit in the scope that
//! creates the lambda. Inlined iterator combinators reduce `xs.iter().map(f)` to
//! exactly this shape, so the chain's closures stop allocating environments and
//! stop dispatching through code slots.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::mir::*;

use crate::scope::{Ownership, ScopeId, ScopeTree};
use crate::visit::{raw, top_level_blocks_from};

fn callee_and_args(inst: &Inst) -> Option<(Idx<Inst>, &[Idx<Inst>])> {
    match inst {
        Inst::CallValue { callee, args } | Inst::CallValueMulti { callee, args, .. } => {
            Some((*callee, args))
        }
        _ => None,
    }
}

type CallSite = (Idx<Block>, usize);

/// Every lambda used only as a callee, mapped to its call positions.
fn lambda_calls(mf: &MirFunc, tree: &ScopeTree) -> HashMap<Idx<Inst>, Vec<CallSite>> {
    let mut uses: HashMap<Idx<Inst>, usize> = HashMap::default();
    let mut calls: HashMap<Idx<Inst>, Vec<CallSite>> = HashMap::default();
    let is_lambda = |id| matches!(mf.insts[id], Inst::Lambda { .. });
    let mut count = |v: Idx<Inst>| {
        if is_lambda(v) {
            *uses.entry(v).or_default() += 1;
        }
    };
    for (b, block) in mf.blocks.iter() {
        if tree.scope_of_block(b).is_none() {
            continue;
        }
        for (pos, &id) in block.insts.iter().enumerate() {
            let inst = &mf.insts[id];
            inst.uses_each(&mut count);
            if let Some((callee, _)) = callee_and_args(inst)
                && is_lambda(callee)
            {
                calls.entry(callee).or_default().push((b, pos));
            }
        }
        block.term.uses_each(&mut count);
    }
    calls.retain(|lambda, sites| uses.get(lambda) == Some(&sites.len()));
    calls
}

/// A by-value capture snapshots its cell when the lambda is created. Inlining
/// reads the cell at the call instead, so a write the call can see without the
/// lambda being created again must block the move.
fn capture_may_change(
    mf: &MirFunc,
    region: &HashSet<Idx<Block>>,
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

fn movable(
    mf: &MirFunc,
    tree: &ScopeTree,
    own: &Ownership,
    body: ScopeId,
    sites: &[CallSite],
    specialized: bool,
) -> bool {
    let scope = &tree.scopes[body];
    let (Some(lambda), Some(creator)) = (scope.lambda, scope.parent) else {
        return false;
    };
    let captured = &own.captures[body];
    if captured.iter().any(|&cell| mf.cells[cell].borrowed) {
        return false;
    }
    let mut min_dests = usize::MAX;
    for &(block, pos) in sites {
        let call = mf.blocks[block].insts[pos];
        let Some((_, args)) = callee_and_args(&mf.insts[call]) else {
            return false;
        };
        if tree.scope_of_block(block) != Some(creator)
            || args.len() != scope.params.len()
            || mf.call_abi_plans.contains_key(&call)
            || mf.flat_call_lanes.contains_key(&call)
            || mf.call_rep_plans.get(&call)
                != mf.lambda_abis.get(&lambda).map(|abi| &abi.representation)
        {
            return false;
        }
        let dests = match &mf.insts[call] {
            Inst::CallValueMulti { dests, .. } => dests.len(),
            _ => 1,
        };
        min_dests = min_dests.min(dests);
    }
    for &block in &scope.region {
        for &id in &mf.blocks[block].insts {
            if matches!(mf.insts[id], Inst::Lambda { .. }) {
                return false;
            }
            if specialized {
                // Moving a writer can change a box shared with another lambda.
                let mut shared_write = false;
                mf.insts[id].cell_writes_each(&mut |cell| {
                    shared_write |= captured.contains(&cell)
                        && own.captures.iter().enumerate().any(|(sid, cells)| {
                            sid != body && sid != creator && cells.contains(&cell)
                        });
                });
                if shared_write {
                    return false;
                }
            }
        }
        match &mf.blocks[block].term {
            Term::Yield { .. } => return false,
            Term::Return(values) if values.len() > min_dests => return false,
            _ => {}
        }
    }
    captured.is_empty()
        || sites.iter().all(|&(block, pos)| {
            !capture_may_change(
                mf,
                &tree.scopes[creator].region,
                captured,
                lambda,
                mf.blocks[block].insts[pos],
            )
        })
}

fn move_body(mf: &mut MirFunc, body: &[Idx<Block>], params: Vec<Idx<Cell>>, site: CallSite) {
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
    mf.blocks[cont].term = std::mem::replace(&mut mf.blocks[block].term, Term::Goto(body[0]));
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

pub(crate) fn inline_lambda_calls(mf: &mut MirFunc, callbacks: &HashSet<Idx<Inst>>) -> bool {
    if skull_mir::pass_disabled("lambda")
        || !mf
            .insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::Lambda { .. }))
    {
        return false;
    }
    let tree = ScopeTree::build(mf);
    let calls = lambda_calls(mf, &tree);
    if calls.is_empty() {
        return false;
    }
    let own = tree.analyze(mf);
    let candidates: Vec<_> = tree
        .scopes
        .iter()
        .enumerate()
        .filter_map(|(sid, scope)| {
            let lambda = scope.lambda?;
            let sites = calls.get(&lambda)?;
            let specialized = callbacks.contains(&lambda);
            ((sites.len() == 1 || specialized) && movable(mf, &tree, &own, sid, sites, specialized))
                .then_some((sid, lambda, sites))
        })
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let mut moved = Vec::new();
    for (sid, lambda, sites) in candidates {
        let (first, rest) = sites.split_first().expect("lambda has a call");
        let Inst::Lambda { params, entry } =
            std::mem::replace(&mut mf.insts[lambda], Inst::Const(Lit::Nil))
        else {
            unreachable!("candidate is a lambda")
        };
        mf.lambda_abis.remove(&lambda);
        mf.code_value_identities.remove(&lambda);
        let body = top_level_blocks_from(mf, entry);
        for &site in rest {
            let (blocks, params) =
                crate::inline::clone_lambda_region(mf, &body, &params, &own.captures[sid]);
            moved.push((blocks, params, site));
        }
        moved.push((body, params, *first));
    }
    // Splitting a block shifts the positions after the call, so move the
    // later calls in a block first.
    moved.sort_by_key(|(_, _, (b, pos))| (raw(*b), std::cmp::Reverse(*pos)));
    for (body, params, site) in moved {
        move_body(mf, &body, params, site);
    }
    true
}
