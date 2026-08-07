//! Whole-program reachability: find the roots, expand the transitive closure,
//! emit only that. Every backend wants this search and they differ only in the
//! search PARAMETERS -- which roots, what counts as a leaf, whether a cycle is
//! legal, and where the successor edges come from. The search itself lives here
//! once; the parameters live in a [`ReachPolicy`]. Plan: `docs/global-dce-plan.md`.
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

use crate::mir::{FuncRef, Inst, MirFunc};
use ustr::Ustr;
use bone_semantic::prelude::CompilerError;
use rustc_hash::FxHashSet as HashSet;
use std::hash::Hash;

/// The per-backend parameters of the reachability search.
///
/// The driver owns the traversal state (seen/in-progress marking, the stack,
/// cycle detection, post-order). The policy owns everything else, including the
/// bodies it accumulates -- `visit` reports only the successor edges, so a
/// policy is free to record whatever else expanding a node produced.
pub trait ReachPolicy {
    /// A unit of work. `Clone + Eq + Hash` because the driver marks every node.
    /// Not `Copy`: JS's specialized-callee node carries a `SpecKey`.
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

/// Every statically-resolved call target in `mf`, walking BLOCK inst-lists (not
/// the raw arena) so a spliced-away call's stale arena inst is not re-discovered
/// -- only the calls that survive inlining co-emit. Hoisted closure bodies are
/// separate `nested_fns` after `convert_closures`, so their calls are walked too.
pub fn call_targets(mf: &MirFunc) -> Vec<FuncRef> {
    fn walk(mf: &MirFunc, out: &mut Vec<FuncRef>) {
        for (_, block) in mf.blocks.iter() {
            for &iid in &block.insts {
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
/// REACHABLE blocks only: a hoisted lambda body is a full arena clone of its
/// parent that differs only in `entry`, so it carries the parent's unreachable
/// blocks and their bindings. Judging those would specialize for a call the body
/// does not contain.
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
    ctx: &crate::bound::BoundCtx,
    mf: &MirFunc,
    out: &mut Vec<crate::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    let empty = bone_semantic::prelude::BoundTree::default();
    for bid in crate::cfg::reachable_blocks_from(mf, mf.entry) {
        for &iid in &mf.blocks[bid].insts {
            let Some(key) = mf.spec_bindings.get(&iid) else { continue };
            let fref = match &mf.insts[iid] {
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
                _ => continue,
            };
            let raw = mf.bound_trees.get(&iid).unwrap_or(&empty);
            let tree = crate::bound::resolve_bound_tree(ws, ctx.inf, ctx.subst, raw, ctx.incoming)?;
            out.push(crate::bound::SpecDemand { func: fref, key: key.clone(), tree });
        }
    }
    Ok(())
}
