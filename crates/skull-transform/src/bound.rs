//! Substituting a placeholder bound-call with its concrete `next`
//! (`docs/monomorphization-plan.md` S 12).
//!
//! ONE rewrite, driven from two places. The tree that resolves a placeholder
//! comes from different records depending on how the bounded body reaches the
//! program -- the SITE's tree when the inliner splices it, the specialization
//! DEMAND's tree when it is emitted standalone -- but the rewrite that consumes
//! the resolved leaf is the same operation either way: retarget the call,
//! record the callee's own specialization, and correct the element the
//! placeholder's fixed `(bool, E)` arity could not describe.

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;
use comfy_la_arena::Idx;
use skull_mir::bound::ResolvedBoundNext;
use skull_mir::lower::{bound_next_shape, BoundNextElem, ElemRebuild, SpecKey};
use skull_mir::mir::*;

/// Resolve every placeholder in a body that is being emitted STANDALONE at one
/// instantiation, from the tree its specialization demand carried.
///
/// The counterpart of the splice path: there the placeholders arrive as clones
/// inside the caller and the tree is the site's, here they are the body's own
/// and the tree is the demand's. Same rewrite, and the two cannot drift because
/// there is only one of it.
///
/// Nested bodies are included: a hoisted lambda or a nested `fn` inside a
/// bounded body carries the same placeholders and resolves through the same
/// tree, and leaving them out makes the residue surface at runtime.
pub fn resolve_standalone_bound_calls(
    ws: &Workspace,
    mf: &mut MirFunc,
    incoming: &skull_mir::bound::ResolvedBoundTree,
) -> Result<(), CompilerError> {
    let ids: Vec<(Idx<Inst>, usize)> = mf
        .insts
        .iter()
        .filter_map(|(id, i)| match i {
            Inst::CallBoundMulti { generic_idx, .. } => Some((id, *generic_idx as usize)),
            _ => None,
        })
        .collect();
    for (id, g) in ids {
        let Some(leaf) = incoming.get(g) else {
            return Err(CompilerError::new_no_location(format!(
                "internal: placeholder bound-call (generic #{g}) has no concrete \
                 resolution in the instantiation `{}` is emitted at",
                mf.name
            )));
        };
        let (split, leaf) = (mf.bound_next_split.contains(&id), leaf.clone());
        substitute_bound_call(mf, id, split, &leaf, ws, "in this instantiation")?;
    }
    for nf in &mut mf.nested_fns {
        resolve_standalone_bound_calls(ws, nf, incoming)?;
    }
    Ok(())
}

/// Rewrite the placeholder [`Inst::CallBoundMulti`] at `call` into a concrete
/// [`Inst::CallMulti`] on `leaf`'s witness.
///
/// `already_split` is the body's own record that the LOWERING sized this
/// placeholder's dests to the real return arity and bound the loop pattern to
/// the leaves ([`MirFunc::bound_next_split`]); it is not derivable from
/// `dests.len()`, because a one-leaf element is two dests either way.
///
/// A `CallerGeneric` leaf is the genuinely unresolved case: the enclosing body
/// is still generic in that position, so nothing here can name a callee.
pub fn substitute_bound_call(
    func: &mut MirFunc,
    call: Idx<Inst>,
    already_split: bool,
    leaf: &ResolvedBoundNext,
    ws: &Workspace,
    what: &str,
) -> Result<(), CompilerError> {
    let (concrete, spec) = match leaf {
        ResolvedBoundNext::Concrete { func, spec, .. } => (*func, spec),
        ResolvedBoundNext::CallerGeneric(g) => {
            return Err(CompilerError::new_no_location(format!(
                "internal: placeholder bound-call (generic #{g}) has no concrete \
                 resolution {what}"
            )));
        }
    };
    let Inst::CallBoundMulti { dests, args, .. } = &func.insts[call] else {
        return Ok(());
    };
    let (dests, args) = (dests.clone(), args.clone());
    func.insts[call] = Inst::CallMulti { dests, func: concrete, args };
    if let Some(key) = spec.clone() {
        func.spec_bindings.insert(call, key);
    }
    // The placeholder's arity is `(bool, E)` because bound conformance
    // fixes it there; the CONCRETE callee is only known here. When its
    // element splats into leaves, the extra returns were being dropped
    // by Lua and `E` bound the first leaf -- so the body then projected
    // a field off a number. Size the dests to what the callee really
    // returns and repack the element, so the body's own view of `E` is
    // unchanged. The repack is a transient: SROA folds the field reads
    // straight to the leaf cells, and a body that never touches the
    // element (`count`) drops it as dead -- which is why this does not
    // reintroduce the box the mark forbids, and why it does not
    // over-reject `count` the way a splice-time arity check did.
    if already_split {
        check_bound_next_split(func, call, ws, concrete, spec.as_ref())
    } else {
        repack_bound_next_element(func, call, ws, concrete, spec.as_ref())
    }
}

/// The lowering already split this placeholder's element into leaf cells, so
/// there is nothing to rebuild -- only the two sides' agreement to confirm.
///
/// They read the same instantiation from opposite ends: the lowering from the
/// loop pattern's binding types, this from the concrete callee's return type. A
/// disagreement means the body binds a different number of leaves than the call
/// returns, which is a positional mis-pairing -- a leaf's value landing in
/// another leaf's cell, silently. Rejected rather than repacked, because a
/// repack over an already-split element would write into a cell nothing reads.
fn check_bound_next_split(
    func: &MirFunc,
    call: Idx<Inst>,
    ws: &Workspace,
    concrete: FuncRef,
    spec: Option<&SpecKey>,
) -> Result<(), CompilerError> {
    let Some(shape) = bound_next_shape(ws, concrete, spec) else {
        return Ok(());
    };
    let Inst::CallMulti { dests, .. } = &func.insts[call] else {
        return Ok(());
    };
    if dests.len() == shape.slot_count {
        return Ok(());
    }
    Err(CompilerError::new_no_location(format!(
        "`{}`: the loop element was split into {} destinations but `next` returns \
         {} values at this specialization -- the two read the same instantiation \
         and must agree, or a leaf's value lands in another leaf's cell",
        func.name,
        dests.len(),
        shape.slot_count
    )))
}

/// Re-size a just-substituted `next` call to the concrete callee's real return
/// arity, and rebuild the element from the extra leaves into the cell the body
/// already reads.
///
/// Only the placeholder path needs this. `lower_for_user_iter` sizes its dests
/// from `ret_shape.slot_count()` at lowering time because it has the concrete
/// callee in hand; a generic combinator body does not, so the correction lands
/// here, at the first point the callee is known.
fn repack_bound_next_element(
    func: &mut MirFunc,
    call: Idx<Inst>,
    ws: &Workspace,
    concrete: FuncRef,
    spec: Option<&SpecKey>,
) -> Result<(), CompilerError> {
    let Some(shape) = bound_next_shape(ws, concrete, spec) else {
        return Ok(());
    };
    let Inst::CallMulti { dests, .. } = &func.insts[call] else {
        return Ok(());
    };
    let dests = dests.clone();
    // Triggered by the element SPLATTING, never by the arity growing. A ONE-leaf
    // element returns `(ok, leaf)` -- the same arity the placeholder assumed -- and
    // still needs the repack, because what lands in the element cell is a bare
    // leaf the body then projects a field off. Gating on arity leaves exactly that
    // case broken, which is the seed's "NOT arity" probe.
    let plan = match &shape.elem {
        BoundNextElem::Opaque => return Ok(()),
        BoundNextElem::Rebuild(plan) => plan,
        // Never rewrite the call without knowing how to put the element back
        // together: the leaves would land in cells nothing reads while the body
        // still projects fields off one of them.
        BoundNextElem::Unsupported => {
            return Err(CompilerError::new_no_location(format!(
                "`{}` returns {} values at this specialization but its element \
                 cannot be rebuilt from leaves (a nullable `nobox` element, whose \
                 presence leaf pairs with no field). Mechanism I of \
                 docs/nobox-case-matrix.md.",
                func.name, shape.slot_count
            )));
        }
    };
    let elem_cell = dests[1];
    let leaf_count = plan.leaf_count();
    if leaf_count + 1 != shape.slot_count {
        return Err(CompilerError::new_no_location(format!(
            "`{}`: `next` returns {} values but its element has {} leaves -- the \
             rebuild is positional over those leaves, so repacking a mismatched \
             pair would write one leaf's value into another field",
            func.name, shape.slot_count, leaf_count
        )));
    }

    // The call keeps `ok` and gains one fresh cell per leaf; the element cell is
    // no longer a destination, it is what the repack stores into.
    let leaf_cells: Vec<Idx<Cell>> = (0..leaf_count).map(|_| func.new_cell(None)).collect();
    let mut new_dests = vec![dests[0]];
    new_dests.extend(leaf_cells.iter().copied());
    if let Inst::CallMulti { dests, .. } = &mut func.insts[call] {
        *dests = new_dests;
    }

    let mut tail: Vec<Idx<Inst>> = Vec::new();
    let mut taken = 0usize;
    let packed = emit_elem_rebuild(func, plan, &leaf_cells, &mut taken, &mut tail);
    let store = func.insts.alloc(Inst::Store { cell: elem_cell, value: packed });

    // Inserted immediately after the call, in its own block: the element cell must
    // hold the rebuilt value before the `ok` branch reads it.
    tail.push(store);
    for (_, block) in func.blocks.iter_mut() {
        if let Some(at) = block.insts.iter().position(|i| *i == call) {
            block.insts.splice(at + 1..at + 1, tail);
            return Ok(());
        }
    }
    Err(CompilerError::new_no_location(format!(
        "internal: substituted `next` call in `{}` is not in any block",
        func.name
    )))
}

/// Emit the rebuild of one element from `cells`, consuming them left to right,
/// appending every inst it creates to `tail` in evaluation order. Returns the
/// value the element cell should hold.
///
/// The rebuild is a TRANSIENT in every arm: copy-prop forwards a field read of a
/// `MakeValueStruct` and a `TupleIndex` of a `MakeTuple` to the construction
/// operand, so the aggregates die in DCE and the `nobox` guarantee still holds
/// over the emitted body.
fn emit_elem_rebuild(
    func: &mut MirFunc,
    plan: &ElemRebuild,
    cells: &[Idx<Cell>],
    taken: &mut usize,
    tail: &mut Vec<Idx<Inst>>,
) -> Idx<Inst> {
    match plan {
        ElemRebuild::Passthrough => {
            let l = func.insts.alloc(Inst::Load(cells[*taken]));
            *taken += 1;
            tail.push(l);
            l
        }
        ElemRebuild::Value(name, fields) => {
            let mut fs = Vec::with_capacity(fields.len());
            for f in fields {
                let l = func.insts.alloc(Inst::Load(cells[*taken]));
                *taken += 1;
                tail.push(l);
                fs.push((*f, l));
            }
            let packed = func.insts.alloc(Inst::MakeValueStruct { name: *name, fields: fs });
            tail.push(packed);
            packed
        }
        ElemRebuild::Tuple(parts) => {
            let mut vs = Vec::with_capacity(parts.len());
            for p in parts {
                vs.push(emit_elem_rebuild(func, p, cells, taken, tail));
            }
            let packed = func.insts.alloc(Inst::MakeTuple(vs));
            tail.push(packed);
            packed
        }
    }
}
