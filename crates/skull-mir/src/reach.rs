use crate::mir::{Block, FuncRef, Inst, MirFunc};
use bone_core::prelude::CompilerError;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet as HashSet};
use ustr::Ustr;

pub struct GlobalReach {
    id_of: FxHashMap<Ustr, u32>,
    /// Indexed by the bit each name owns in a closure set.
    globals: Vec<Ustr>,
    component_of: Vec<u32>,
    /// One bitset per component: the globals its bodies read, transitively.
    closure: Vec<Vec<u64>>,
}

impl GlobalReach {
    pub fn deps_of(&self, body: Ustr) -> Vec<Ustr> {
        let Some(&id) = self.id_of.get(&body) else {
            return Vec::new();
        };
        let bits = &self.closure[self.component_of[id as usize] as usize];
        let mut out = Vec::new();
        for (word_index, &word) in bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                out.push(self.globals[word_index * 64 + bit]);
                word &= word - 1;
            }
        }
        out
    }
}

/// One closure per component: Tarjan returns callees first, so each component
/// unions its callees' completed closures once.
pub fn transitive_global_reads<G: AsRef<[Ustr]>, C: AsRef<[Ustr]>>(
    direct_globals: &FxHashMap<Ustr, G>,
    direct_calls: &FxHashMap<Ustr, C>,
) -> GlobalReach {
    let mut id_of: FxHashMap<Ustr, u32> = FxHashMap::default();
    let mut intern = |name: Ustr| -> u32 {
        let next = id_of.len() as u32;
        *id_of.entry(name).or_insert(next)
    };
    let mut global_bit: FxHashMap<Ustr, u32> = FxHashMap::default();
    let mut globals = Vec::new();
    let mut own_globals: Vec<(u32, u32)> = Vec::new();
    for (body, read) in direct_globals {
        let body = intern(*body);
        for global in read.as_ref() {
            let next = globals.len() as u32;
            let bit = *global_bit.entry(*global).or_insert_with(|| {
                globals.push(*global);
                next
            });
            own_globals.push((body, bit));
        }
    }
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for (body, callees) in direct_calls {
        let body = intern(*body);
        for callee in callees.as_ref() {
            edges.push((body, intern(*callee)));
        }
    }
    // A global's name may never appear as a body; give every name a node so
    // `component_of` is total over `id_of`.
    for global in &globals {
        intern(*global);
    }
    let node_count = id_of.len();
    let mut callees = vec![Vec::new(); node_count];
    for (from, to) in edges {
        callees[from as usize].push(to);
    }

    let components = strongly_connected_components(&callees);
    let mut component_of = vec![u32::MAX; node_count];
    for (index, members) in components.iter().enumerate() {
        for &member in members {
            component_of[member as usize] = index as u32;
        }
    }
    let words = globals.len().div_ceil(64);
    let mut closure = vec![vec![0u64; words]; components.len()];
    for (body, bit) in own_globals {
        let set = &mut closure[component_of[body as usize] as usize];
        set[bit as usize / 64] |= 1 << (bit % 64);
    }
    // Components come out callee-first, so a callee's set is final here.
    for (index, members) in components.iter().enumerate() {
        for &member in members {
            for &callee in &callees[member as usize] {
                let callee_component = component_of[callee as usize] as usize;
                if callee_component == index {
                    continue;
                }
                let (earlier, later) = closure.split_at_mut(index);
                let (from, into) = if callee_component < index {
                    (&earlier[callee_component], &mut later[0])
                } else {
                    unreachable!("Tarjan emits a callee's component before its caller's");
                };
                for (word, source) in into.iter_mut().zip(from) {
                    *word |= source;
                }
            }
        }
    }
    GlobalReach {
        id_of,
        globals,
        component_of,
        closure,
    }
}

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
/// recursion: the stuck names come back in source order.
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
                .filter(|dep| *dep != init.name && known.contains(dep))
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
            return Err(next.into_iter().map(|(name, _, _)| name).collect());
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
    out: &mut Vec<crate::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    let empty = bone_core::prelude::BoundTree::default();
    let empty_resolved = crate::bound::ResolvedBoundTree::default();
    for bid in emitted_blocks(mf) {
        for &iid in &mf.blocks[bid].insts {
            let Some(inst) = mf.spec_bindings.get(&iid) else {
                continue;
            };
            let fref = match &mf.insts[iid] {
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
                _ => continue,
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
            // An already-resolved tree from a standalone substitution wins: this body's
            // arena cannot state a tree from another instantiation.
            let raw = mf.bound_trees.get(&iid).unwrap_or(&empty);
            let tree = match mf.resolved_bound_trees.get(&iid) {
                Some(t) => t.clone(),
                None if !raw.is_empty() => {
                    return Err(CompilerError::new_no_location(
                        "concrete specialized call has no resolved bound plan".to_string(),
                    ));
                }
                None => empty_resolved.clone(),
            };
            out.push(crate::bound::SpecDemand {
                inst: inst.clone(),
                tree,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod global_reach_tests {
    use super::transitive_global_reads;
    use rustc_hash::FxHashMap;
    use ustr::Ustr;

    fn table(rows: &[(&str, &[&str])]) -> FxHashMap<Ustr, Vec<Ustr>> {
        rows.iter()
            .map(|(name, items)| {
                (
                    Ustr::from(name),
                    items.iter().map(|item| Ustr::from(item)).collect(),
                )
            })
            .collect()
    }

    fn sorted(mut names: Vec<Ustr>) -> Vec<&'static str> {
        names.sort();
        names.into_iter().map(|name| name.as_str()).collect()
    }

    #[test]
    fn reads_follow_calls_through_chains_and_cycles() {
        let reach = transitive_global_reads(
            &table(&[("a", &["G0"]), ("c", &["G1"]), ("d", &["G2"])]),
            &table(&[
                ("entry", &["a"]),
                ("a", &["b"]),
                ("b", &["c"]),
                ("c", &["b"]),
            ]),
        );
        assert_eq!(sorted(reach.deps_of(Ustr::from("entry"))), ["G0", "G1"]);
        assert_eq!(sorted(reach.deps_of(Ustr::from("a"))), ["G0", "G1"]);
        assert_eq!(sorted(reach.deps_of(Ustr::from("b"))), ["G1"]);
        assert_eq!(sorted(reach.deps_of(Ustr::from("c"))), ["G1"]);
        assert_eq!(sorted(reach.deps_of(Ustr::from("d"))), ["G2"]);
        assert!(reach.deps_of(Ustr::from("G0")).is_empty());
        assert!(reach.deps_of(Ustr::from("unknown")).is_empty());
    }
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
                init("DERIVED", &["__nbg_BASE_1", "UNINITIALIZED", "DERIVED"]),
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
            vec![init("A", &["B"]), init("C", &[]), init("B", &["A"])],
            &FxHashMap::default(),
        )
        .unwrap_err();
        assert_eq!(stuck, [Ustr::from("A"), Ustr::from("B")]);
    }
}
