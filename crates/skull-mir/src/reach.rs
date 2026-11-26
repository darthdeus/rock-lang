use crate::instance::Instance;
use crate::mir::{Block, FuncRef, Inst, MirFunc};
use bone_core::prelude::CompilerError;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet as HashSet};
use ustr::Ustr;

/// In finish order: every component appears after each component it has an
/// edge into.
pub fn strongly_connected_components(callees: &[Vec<u32>]) -> Vec<Vec<u32>> {
    const UNVISITED: u32 = u32::MAX;
    let n = callees.len();
    let mut index_of = vec![UNVISITED; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut components = Vec::new();
    let mut next_index = 0u32;
    // (node, position of the next callee to visit)
    let mut work: Vec<(u32, usize)> = Vec::new();
    for root in 0..n {
        if index_of[root] != UNVISITED {
            continue;
        }
        work.push((root as u32, 0));
        index_of[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root as u32);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut position)) = work.last_mut() {
            let node_usize = node as usize;
            if *position < callees[node_usize].len() {
                let callee = callees[node_usize][*position] as usize;
                *position += 1;
                if index_of[callee] == UNVISITED {
                    index_of[callee] = next_index;
                    low[callee] = next_index;
                    next_index += 1;
                    stack.push(callee as u32);
                    on_stack[callee] = true;
                    work.push((callee as u32, 0));
                } else if on_stack[callee] {
                    low[node_usize] = low[node_usize].min(index_of[callee]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                let parent = parent as usize;
                low[parent] = low[parent].min(low[node_usize]);
            }
            if low[node_usize] == index_of[node_usize] {
                let mut members = Vec::new();
                loop {
                    let member = stack.pop().expect("component member on the Tarjan stack");
                    on_stack[member as usize] = false;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                components.push(members);
            }
        }
    }
    components
}

/// Overloads merge under one name, which at worst over-approximates a
/// dependency.
pub fn symbol_references(ws: &bone_core::workspace::Workspace) -> FxHashMap<Ustr, HashSet<Ustr>> {
    use bone_core::hir::BodyOwner;
    let mut deps: FxHashMap<Ustr, HashSet<Ustr>> = FxHashMap::default();
    for module in ws.source_modules() {
        let Some(inf) = ws.inference(module.id) else {
            continue;
        };
        for (fid, f) in module.functions.iter() {
            if let Some(bi) = inf.per_fn.get(&BodyOwner::Function(fid)) {
                deps.entry(f.name)
                    .or_default()
                    .extend(bi.deps.iter().copied());
            }
        }
        for (gid, g) in module.globals.iter() {
            if let Some(bi) = inf.per_fn.get(&BodyOwner::Global(gid)) {
                deps.entry(g.name)
                    .or_default()
                    .extend(bi.deps.iter().copied());
            }
        }
    }
    deps
}

pub fn forward_reach(deps: &FxHashMap<Ustr, HashSet<Ustr>>, start: Ustr) -> HashSet<Ustr> {
    let mut seen: HashSet<Ustr> = HashSet::default();
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        if let Some(ds) = deps.get(&n) {
            for &d in ds {
                if seen.insert(d) {
                    stack.push(d);
                }
            }
        }
    }
    seen
}

pub struct PendingInit<T> {
    pub name: Ustr,
    /// Transitive through the call graph. Leaf names and names without an
    /// initializer are allowed.
    pub deps: Vec<Ustr>,
    pub item: T,
}

/// Each init runs after every initialized global it reads; rounds keep source
/// order. `leaf_owner` maps `nobox` leaf names to their global, or the dependent
/// init would read undefined leaves. A round without progress means mutual
/// recursion: names that participate in a cycle come back in source order.
pub fn order_global_inits<T>(
    inits: Vec<PendingInit<T>>,
    leaf_owner: &FxHashMap<Ustr, Ustr>,
) -> Result<Vec<T>, Vec<Ustr>> {
    let known: HashSet<Ustr> = inits.iter().map(|init| init.name).collect();
    let mut remaining: Vec<(Ustr, HashSet<Ustr>, T)> = inits
        .into_iter()
        .map(|init| {
            let deps = init
                .deps
                .iter()
                .map(|dep| leaf_owner.get(dep).copied().unwrap_or(*dep))
                .filter(|dep| known.contains(dep))
                .collect();
            (init.name, deps, init.item)
        })
        .collect();
    let mut done: HashSet<Ustr> = HashSet::default();
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for (name, deps, item) in remaining {
            if deps.iter().all(|dep| done.contains(dep)) {
                done.insert(name);
                ordered.push(item);
            } else {
                next.push((name, deps, item));
            }
        }
        if next.len() == before {
            let indices: FxHashMap<_, _> = next
                .iter()
                .enumerate()
                .map(|(i, (name, _, _))| (*name, i as u32))
                .collect();
            let edges: Vec<Vec<_>> = next
                .iter()
                .map(|(_, deps, _)| {
                    deps.iter()
                        .filter_map(|dep| indices.get(dep).copied())
                        .collect()
                })
                .collect();
            let mut cyclic = vec![false; next.len()];
            for component in strongly_connected_components(&edges) {
                if component.len() > 1 || edges[component[0] as usize].contains(&component[0]) {
                    for node in component {
                        cyclic[node as usize] = true;
                    }
                }
            }
            return Err(next
                .into_iter()
                .zip(cyclic)
                .filter_map(|((name, _, _), cyclic)| cyclic.then_some(name))
                .collect());
        }
        remaining = next;
    }
    Ok(ordered)
}

/// Regions entered on their own: the body from `mf.entry`, every `Inst::Lambda`
/// named by an emitted block, and every `coro_defers` entry. Not every arena
/// block: a hoisted lambda body is an arena clone carrying its parent's blocks.
pub fn emitted_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen = vec![false; mf.blocks.len()];
    let mut out: Vec<Idx<Block>> = Vec::new();
    let mut regions: Vec<Idx<Block>> = vec![mf.entry];
    regions.extend(mf.coro_defers.iter().map(|&(_, entry)| entry));
    while let Some(entry) = regions.pop() {
        if crate::cfg::dense::bx(entry) >= mf.blocks.len() {
            continue;
        }
        for bid in crate::cfg::reachable_blocks_from(mf, entry) {
            if std::mem::replace(&mut seen[crate::cfg::dense::bx(bid)], true) {
                continue;
            }
            out.push(bid);
            for &iid in &mf.blocks[bid].insts {
                // `CoroHook` names its body through an operand, not the block list.
                let lambda = match &mf.insts[iid] {
                    Inst::CoroHook { lambda, .. } => *lambda,
                    _ => iid,
                };
                if let Inst::Lambda { entry, .. } = &mf.insts[lambda] {
                    regions.push(*entry);
                }
            }
        }
    }
    out
}

/// Deduped in source order. A top-level fn used as a value lowers to
/// `Inst::Global`, not `Inst::Call`, so [`call_targets`] alone misses it.
pub fn global_ref_names(mf: &MirFunc) -> Vec<Ustr> {
    fn walk(mf: &MirFunc, out: &mut Vec<Ustr>, seen: &mut HashSet<Ustr>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                let name = match &mf.insts[iid] {
                    Inst::Global(n) => *n,
                    Inst::StoreGlobal { name, .. } => *name,
                    _ => continue,
                };
                if seen.insert(name) {
                    out.push(name);
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::default();
    walk(mf, &mut out, &mut seen);
    out
}

/// Walks block lists, including unreachable artifacts, unlike [`emitted_blocks`].
/// Calls with `spec_bindings` target specialized bodies and keep nothing alive.
pub fn call_targets(mf: &MirFunc) -> Vec<FuncRef> {
    fn walk(mf: &MirFunc, out: &mut Vec<FuncRef>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
                if mf.spec_bindings.contains_key(&iid) {
                    continue;
                }
                match &mf.insts[iid] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => out.push(*func),
                    _ => {}
                }
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out);
        }
    }
    let mut out = Vec::new();
    walk(mf, &mut out);
    out
}

/// Emitted blocks only: a hoisted lambda's arena clone carries its parent's
/// unreachable bindings. Resolve each bound tree in the body owning its arena.
pub fn spec_bound_calls(
    ws: &bone_core::workspace::Workspace,
    mf: &MirFunc,
    out: &mut Vec<Instance>,
) -> Result<(), CompilerError> {
    if mf.spec_bindings.is_empty() {
        return Ok(());
    }
    for bid in emitted_blocks(mf) {
        for &iid in &mf.blocks[bid].insts {
            out.extend(spec_bound_call(ws, mf, iid)?.cloned());
        }
    }
    Ok(())
}

pub fn spec_bound_call<'a>(
    ws: &bone_core::workspace::Workspace,
    mf: &'a MirFunc,
    iid: Idx<Inst>,
) -> Result<Option<&'a Instance>, CompilerError> {
    let Some(inst) = mf.spec_bindings.get(&iid) else {
        return Ok(None);
    };
    let fref = match &mf.insts[iid] {
        Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
        _ => return Ok(None),
    };
    // Check the delivery identity first, so a missing bound fact cannot mask an
    // owner disagreement.
    let owner_agrees =
        inst.func == fref && mf.callee_instances.get(&iid).is_some_and(|oi| oi == inst);
    if !owner_agrees {
        return Err(CompilerError::new_no_location(format!(
            "specialized call to `{}` in `{}` has no concrete owner agreeing with its \
             delivery instance",
            ws.module(fref.module).functions[fref.func].name,
            mf.name,
        )));
    }
    if !mf.resolved_bound_trees.contains_key(&iid)
        && mf
            .bound_trees
            .get(&iid)
            .is_some_and(|tree| !tree.is_empty())
    {
        return Err(CompilerError::new_no_location(
            "concrete specialized call has no resolved bound plan".to_string(),
        ));
    }
    Ok(Some(inst))
}

#[cfg(test)]
mod init_order_tests {
    use super::{PendingInit, order_global_inits};
    use rustc_hash::FxHashMap;
    use ustr::Ustr;

    fn init(name: &str, deps: &[&str]) -> PendingInit<&'static str> {
        PendingInit {
            name: Ustr::from(name),
            deps: deps.iter().map(|d| Ustr::from(d)).collect(),
            item: Ustr::from(name).as_str(),
        }
    }

    #[test]
    fn a_leaf_read_orders_after_the_leaf_owner() {
        let leaf_owner: FxHashMap<Ustr, Ustr> = [
            (Ustr::from("__nbg_BASE_0"), Ustr::from("BASE")),
            (Ustr::from("__nbg_BASE_1"), Ustr::from("BASE")),
        ]
        .into_iter()
        .collect();
        let order = order_global_inits(
            vec![
                init("DERIVED", &["__nbg_BASE_1", "UNINITIALIZED"]),
                init("BASE", &[]),
                init("OTHER", &[]),
            ],
            &leaf_owner,
        )
        .unwrap();
        assert_eq!(order, ["BASE", "OTHER", "DERIVED"]);
    }

    #[test]
    fn mutually_recursive_inits_report_the_stuck_names() {
        let stuck = order_global_inits(
            vec![
                init("A", &["B"]),
                init("C", &["A"]),
                init("B", &["A"]),
                init("D", &["D"]),
            ],
            &FxHashMap::default(),
        )
        .unwrap_err();
        assert_eq!(stuck, [Ustr::from("A"), Ustr::from("B"), Ustr::from("D")]);
    }
}
