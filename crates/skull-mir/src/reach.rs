//! Whole-program reachability: find the roots, expand the transitive closure,
//! emit only that. Every backend wants this search and they differ only in the
//! search PARAMETERS -- which roots, what counts as a leaf, whether a cycle is
//! legal, and where the successor edges come from. The search itself lives here
//! once; the parameters live in a [`ReachPolicy`].
//!
//! Not to be confused with function-local DCE (`skull_transform::dce`), which
//! prunes instructions inside one body. This is the layer above: call-graph
//! reachability at whole-node granularity.
//!
//! The node type is an associated type rather than a bare `FuncRef` because the
//! JS search walks TWO node kinds -- functions and the module-global initializers
//! those functions read, each able to discover the other -- while GLSL walks only
//! functions. One search, two node vocabularies.
//!
//! Expansion is an explicit stack, not Rust recursion: the post-order is
//! identical (children are pushed in reverse so they pop in order, and a node's
//! exit frame sits below its children), and a deep call graph cannot blow the
//! native stack.

use crate::mir::{Block, FuncRef, Inst, MirFunc};
use bone_semantic::prelude::CompilerError;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet as HashSet};
use std::hash::Hash;
use ustr::Ustr;

pub struct GlobalReach {
    id_of: FxHashMap<Ustr, u32>,
    names: Vec<Ustr>,
    closure: Vec<HashSet<u32>>,
}

impl GlobalReach {
    pub fn deps_of(&self, body: Ustr) -> Vec<Ustr> {
        let Some(&id) = self.id_of.get(&body) else {
            return Vec::new();
        };
        self.closure[id as usize]
            .iter()
            .map(|&global| self.names[global as usize])
            .collect()
    }
}

pub fn transitive_global_reads(
    direct_globals: &FxHashMap<Ustr, Vec<Ustr>>,
    direct_calls: &FxHashMap<Ustr, Vec<Ustr>>,
) -> GlobalReach {
    let mut id_of = FxHashMap::default();
    let mut names = Vec::new();
    let intern = |name: Ustr, id_of: &mut FxHashMap<Ustr, u32>, names: &mut Vec<Ustr>| match id_of
        .get(&name)
    {
        Some(&index) => index,
        None => {
            let index = names.len() as u32;
            names.push(name);
            id_of.insert(name, index);
            index
        }
    };
    for (body, globals) in direct_globals {
        intern(*body, &mut id_of, &mut names);
        for global in globals {
            intern(*global, &mut id_of, &mut names);
        }
    }
    for (body, callees) in direct_calls {
        intern(*body, &mut id_of, &mut names);
        for callee in callees {
            intern(*callee, &mut id_of, &mut names);
        }
    }
    let mut closure = vec![HashSet::default(); names.len()];
    let mut callees = vec![Vec::new(); names.len()];
    for (body, globals) in direct_globals {
        let body = id_of[body] as usize;
        for global in globals {
            closure[body].insert(id_of[global]);
        }
    }
    for (body, direct) in direct_calls {
        let body = id_of[body] as usize;
        callees[body].extend(direct.iter().map(|callee| id_of[callee]));
    }
    let mut additions = Vec::new();
    loop {
        let mut changed = false;
        for body in 0..closure.len() {
            additions.clear();
            for &callee in &callees[body] {
                for &global in &closure[callee as usize] {
                    if !closure[body].contains(&global) {
                        additions.push(global);
                    }
                }
            }
            for &global in &additions {
                changed |= closure[body].insert(global);
            }
        }
        if !changed {
            break;
        }
    }
    GlobalReach {
        id_of,
        names,
        closure,
    }
}

/// One global initializer waiting for its slot in load-time order.
pub struct PendingInit<T> {
    /// The global's logical name.
    pub name: Ustr,
    /// The globals the initializer reads, transitively through its call graph.
    /// Leaf names and names without an initializer are allowed here.
    pub deps: Vec<Ustr>,
    /// The backend's payload for this init, returned in sorted order.
    pub item: T,
}

/// Order global initializers so each runs after every initialized global it
/// reads. Rounds keep source order among the inits that become ready together.
///
/// A `nobox` global is read through its synthesized leaf names, so `leaf_owner`
/// maps each leaf back to the global that owns it; without it the dependent init
/// would sort first and read undefined leaves. Self-reads and reads of globals
/// with no initializer impose nothing. A round without progress means the
/// initializers are mutually recursive: the stuck names come back as the error,
/// in source order, for the backend to report.
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

/// The per-backend parameters of the reachability search.
///
/// The driver owns the traversal state (seen/in-progress marking, the stack,
/// cycle detection, post-order). The policy owns everything else, including the
/// bodies it accumulates -- `visit` reports only the successor edges, so a
/// policy is free to record whatever else expanding a node produced.
pub trait ReachPolicy {
    /// A unit of work. `Clone + Eq + Hash` because the driver marks every node.
    /// Not `Copy`: JS's specialized-callee node carries an `Instance`.
    type Node: Clone + Eq + Hash;

    /// Where the search starts. Called once, before any expansion.
    fn roots(&mut self) -> Result<Vec<Self::Node>, CompilerError>;

    /// Nodes already on the expansion path when the search starts, so reaching
    /// one is a cycle. GLSL's shader entry is lowered and profile-checked by its
    /// caller and then hands the driver its callees; a callee calling back into
    /// that entry is recursion exactly as any other cycle is, and without this
    /// the driver could not see it.
    fn initially_in_progress(&mut self) -> Vec<Self::Node> {
        Vec::new()
    }

    /// A node that is never expanded and never enters the post-order: an
    /// intrinsic, a host-mapped fn, an operator rendered inline at its call
    /// site. Consulted BEFORE the cycle check, so a policy that rejects an
    /// unemittable node from here reports that rather than a recursion error.
    fn is_leaf(&mut self, node: Self::Node) -> Result<bool, CompilerError>;

    /// Expand one node -- lower it, transform it, record it -- and report the
    /// successors it reaches. Called at most once per node.
    fn visit(&mut self, node: Self::Node) -> Result<Vec<Self::Node>, CompilerError>;

    /// A node reached while it is still being expanded, i.e. recursion. Return
    /// `Ok(())` to allow it (the node is simply not expanded twice); return an
    /// error to reject, as GLSL does.
    fn on_cycle(&mut self, node: Self::Node) -> Result<(), CompilerError>;
}

enum Frame<N> {
    Enter(N),
    Exit(N),
}

/// Expand the transitive closure of `policy`'s roots, returning the expanded
/// nodes in post-order (successors before the node that reached them). Leaves
/// are marked but never returned.
///
/// Post-order is mandatory for GLSL, which emits straight from it and has no
/// forward declarations. It is inert for the backends that canonicalize their
/// own emission order.
pub fn walk<P: ReachPolicy>(policy: &mut P) -> Result<Vec<P::Node>, CompilerError> {
    let mut in_progress: HashSet<P::Node> = HashSet::default();
    let mut done: HashSet<P::Node> = HashSet::default();
    let mut order: Vec<P::Node> = Vec::new();

    in_progress.extend(policy.initially_in_progress());
    let roots = policy.roots()?;
    let mut stack: Vec<Frame<P::Node>> = roots.into_iter().rev().map(Frame::Enter).collect();

    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Enter(node) => {
                if done.contains(&node) {
                    continue;
                }
                if policy.is_leaf(node.clone())? {
                    done.insert(node);
                    continue;
                }
                if in_progress.contains(&node) {
                    policy.on_cycle(node)?;
                    continue;
                }
                in_progress.insert(node.clone());
                let succs = policy.visit(node.clone())?;
                stack.push(Frame::Exit(node));
                stack.extend(succs.into_iter().rev().map(Frame::Enter));
            }
            Frame::Exit(node) => {
                in_progress.remove(&node);
                done.insert(node.clone());
                order.push(node);
            }
        }
    }
    Ok(order)
}

/// THE blocks of `mf` that emission renders -- one oracle, because "which
/// blocks count" was answered three different ways and two of them were wrong.
///
/// A body is not one CFG. It is a set of REGIONS, each entered on its own:
///
/// - the body proper, reachable from `mf.entry`;
/// - every `Inst::Lambda` region reached from a block already in the set,
///   transitively. In an ordinary fn `convert_closures` has already hoisted
///   these into `nested_fns` and none remain; in a `co fn` they stay in the
///   arena by design, including the lifecycle-hook bodies `transform_coroutine`
///   peels to the constructor;
/// - every `coro_defers` region.
///
/// That is the same set `coroutine_emit` renders -- the step, its hoisted
/// siblings, one aux fn per defer entry, and one per hook lambda entry. The
/// defer arm is completeness against THAT list, not a measured fix: deleting it
/// leaves every gate green, so a defer region is reachable some other way
/// today. The hook arm is the opposite -- delete it and the hook fixture goes
/// red on both backends.
///
/// NOT every block in the arena, which is the tempting shortcut and is
/// incorrect: a hoisted lambda body is a full arena clone of its parent that
/// differs only in `entry`, so it carries the parent's unreachable blocks, and
/// judging those attributes to this body calls it does not contain.
///
/// Reaching a lambda through the blocks that MENTION it is what separates the
/// two: a live region is named by an `Inst::Lambda` in a block that is itself
/// emitted, and a clone's leftovers are named by nothing.
pub fn emitted_blocks(mf: &MirFunc) -> Vec<Idx<Block>> {
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    let mut out: Vec<Idx<Block>> = Vec::new();
    let mut regions: Vec<Idx<Block>> = vec![mf.entry];
    regions.extend(mf.coro_defers.iter().map(|&(_, entry)| entry));
    while let Some(entry) = regions.pop() {
        if !mf.blocks.iter().any(|(id, _)| id == entry) {
            continue;
        }
        for bid in crate::cfg::reachable_blocks_from(mf, entry) {
            if !seen.insert(bid) {
                continue;
            }
            out.push(bid);
            for &iid in &mf.blocks[bid].insts {
                // `CoroHook` names its body through an operand rather than by
                // sitting in the block list, so following only the block's own
                // `Inst::Lambda`s reaches a `co fn`'s ordinary lambdas and
                // misses every lifecycle hook.
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

/// Every module-global name `mf` reads or writes, main body and hoisted nested
/// fns, deduped in source order.
///
/// A backend needs this for reachability as well as for globals: a top-level fn
/// used as a VALUE (`let f = helper`, a fn passed as an argument) lowers to an
/// `Inst::Global`, NOT an `Inst::Call`, so [`call_targets`] alone does not see it
/// and the referenced fn would be pruned out from under a live reference.
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

/// Every call target in `mf` that is reached through the callee's DECLARATION
/// body, walking BLOCK inst-lists (not the raw arena) so a spliced-away call's
/// stale arena inst is not re-discovered -- only the calls that survive inlining
/// co-emit. Hoisted closure bodies are separate `nested_fns` after
/// `convert_closures`, so their calls are walked too.
///
/// Deliberately NOT [`emitted_blocks`], and the asymmetry is the point: this
/// walk decides what stays ALIVE, where over-approximating costs an unused
/// definition, while `spec_bound_calls` decides what gets DEFINED, where
/// under-approximating ships a call to nothing. Measured over the 18 bin
/// projects plus `tests/` -- 260,154 bodies, 260,076 agreeing as sets, and in
/// `keys` and `run_into` the oracle misses a callee this walk finds. Until
/// those two are explained, the safe side of the asymmetry is where each walk
/// already sits. (The 2,876 multiset disagreements are just the clone re-walk:
/// a hoisted body is a full arena copy, so its parent's calls are counted
/// twice, and reachability dedups them anyway.)
///
/// A site carrying a `spec_bindings` entry names a SPECIALIZED body, and
/// [`spec_bound_calls`] emits that edge as a `SpecDemand`. Yielding its
/// declaration here as well would keep the callee's shared body reachable on
/// the strength of a call that does not target it -- which is how a generic
/// declaration stayed live with every one of its call sites specialized. The
/// two functions partition one edge set; neither is the whole of it.
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

/// Every `(callee, instantiation)` pair `mf` still asks for after inlining: the
/// `spec_bindings` entries the inliner did not splice away. A residual one is a
/// call the SHARED body cannot serve -- its value args are splatted to leaves,
/// and a column-stored element's storage has no `_data` the shared body could
/// read -- so each pair needs its own standalone emitted copy.
///
/// EMITTED blocks only ([`emitted_blocks`]): a hoisted lambda body is a full
/// arena clone of its parent that differs only in `entry`, so it carries the
/// parent's unreachable blocks and their bindings. Judging those would
/// specialize for a call the body does not contain.
///
/// Entry-reachability alone was too NARROW in the other direction, and silently:
/// a `co fn`'s lambda and lifecycle-hook regions are entered through
/// `Inst::Lambda`, never a terminator, so their demands were never filed while
/// their ordinary callees were discovered normally. The emitted chunk then
/// called a specialization nothing defined -- a nil global on luajit, a refusal
/// to emit on JS.
///
/// Per BODY, not per tree -- the caller recurses into `nested_fns` itself. A
/// nested `fn` and a `co fn`'s state machine are their own emitted callables, and
/// leaving them unchecked makes a missing splice silent (the nested fn dies at
/// runtime on a nil `_data`).
/// Each demand also carries the bound-conformance tree the callee's own
/// placeholders resolve through, resolved HERE -- in the body that records it,
/// where its inference arena is in scope. That is what lets a demand travel to
/// a standalone emission at all: a raw [`bone_semantic::prelude::BoundTree`] is
/// stated in this body's `TyId`s and means nothing anywhere else.
pub fn spec_bound_calls(
    ws: &bone_semantic::inference::Workspace,
    mf: &MirFunc,
    out: &mut Vec<crate::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    let empty = bone_semantic::prelude::BoundTree::default();
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
            // Check the delivery identity before reading the bound plan. A
            // broken owner is the primary invariant at this site; otherwise a
            // missing secondary bound fact would mask the owner disagreement.
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
            // An already-RESOLVED tree wins: this call was retargeted by a
            // standalone substitution, whose leaf carried the callee's own
            // resolution. Re-deriving it from `bound_trees` is not possible --
            // there is no raw entry, and this body's arena cannot state a tree
            // that came from another instantiation.
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
