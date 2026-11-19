//! Resolve bounded iterator calls and adapt their concrete return layout.

use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::bound::ResolvedBoundNext;
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BoundNextElem, ElemRebuild, bound_next_shape_from_concrete};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;

pub fn resolve_standalone_bound_calls(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    interner: &MonoInterner,
    mf: &mut MirFunc,
    incoming: &skull_mir::bound::ResolvedBoundTree,
    concrete_plans: &ConcretePlans,
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
        // A bounded witness's own demand needs its subtree.
        if let ResolvedBoundNext::Concrete { sub, .. } = &leaf {
            if !sub.is_empty() {
                mf.resolved_bound_trees.insert(id, sub.clone());
            }
        }
        substitute_bound_call(
            mf,
            id,
            split,
            &leaf,
            scx,
            ws,
            interner,
            concrete_plans,
            "in this instantiation",
        )?;
    }
    for nf in &mut mf.nested_fns {
        resolve_standalone_bound_calls(ws, scx, interner, nf, incoming, concrete_plans)?;
    }
    Ok(())
}

/// Use the lowering's split marker: a one-leaf element has the same
/// destination count as an unsplit element.
pub(crate) fn substitute_bound_call(
    func: &mut MirFunc,
    call: Idx<Inst>,
    already_split: bool,
    leaf: &ResolvedBoundNext,
    scx: &skull_mir::lower::ScalarCtx,
    ws: &Workspace,
    interner: &MonoInterner,
    concrete_plans: &ConcretePlans,
    what: &str,
) -> Result<(), CompilerError> {
    let owner = match leaf {
        ResolvedBoundNext::Concrete { instance, .. } => instance.clone(),
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
    func.insts[call] = Inst::CallMulti {
        dests,
        func: owner.func,
        args,
    };
    skull_mir::site::record_call_owner(func, call, &owner);
    let abi = skull_mir::lower::owner_abi_plan(ws, scx, concrete_plans, &owner)?;
    func.call_abi_plans.insert(call, abi);
    if !owner.args.is_empty() {
        func.spec_bindings.insert(call, owner.clone());
    }
    // Repack a split result for a body that expects the whole iterator element.
    let inst = owner;
    if already_split {
        check_bound_next_split(func, call, scx, ws, interner, &inst, concrete_plans)
    } else {
        repack_bound_next_element(func, call, scx, ws, interner, &inst, concrete_plans)
    }
}

fn bound_next_shape_for_instance(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    interner: &MonoInterner,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<skull_mir::lower::BoundNextShape, CompilerError> {
    if !skull_mir::lower::requires_concrete_body_plan(ws, inst) {
        return Err(CompilerError::new_no_location(
            "internal: a bound witness resolved to an opaque boundary without a concrete result shape",
        ));
    }
    let plan = concrete_plans.get(inst).ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "internal: concrete body plan is missing for bound witness `{}`",
            inst.func.func.into_raw().into_u32()
        ))
    })?;
    bound_next_shape_from_concrete(ws, scx, interner, &plan.signature)
        .map_err(|error| CompilerError::new_no_location(error.message))
}

fn check_bound_next_split(
    func: &MirFunc,
    call: Idx<Inst>,
    scx: &skull_mir::lower::ScalarCtx,
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let shape = bound_next_shape_for_instance(ws, scx, interner, inst, concrete_plans)?;
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

fn repack_bound_next_element(
    func: &mut MirFunc,
    call: Idx<Inst>,
    scx: &skull_mir::lower::ScalarCtx,
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let shape = bound_next_shape_for_instance(ws, scx, interner, inst, concrete_plans)?;
    let Inst::CallMulti { dests, .. } = &func.insts[call] else {
        return Ok(());
    };
    let dests = dests.clone();
    // A one-leaf element has the same arity before and after splitting.
    let plan = match &shape.elem {
        BoundNextElem::Opaque => return Ok(()),
        BoundNextElem::Rebuild(plan) => plan,
        BoundNextElem::Unsupported => {
            return Err(CompilerError::new_no_location(format!(
                "`{}` returns {} values at this specialization but its element \
                 cannot be rebuilt from leaves (a nullable `nobox` element, whose \
                 presence leaf pairs with no field).",
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
    let packed = emit_elem_rebuild(func, plan, &leaf_cells, &mut taken, &mut tail)?;
    let store = func.alloc_inst(Inst::Store {
        cell: elem_cell,
        value: packed,
    });

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

fn emit_elem_rebuild(
    func: &mut MirFunc,
    plan: &ElemRebuild,
    cells: &[Idx<Cell>],
    taken: &mut usize,
    tail: &mut Vec<Idx<Inst>>,
) -> Result<Idx<Inst>, CompilerError> {
    match plan {
        ElemRebuild::Passthrough => {
            let l = func.alloc_inst(Inst::Load(cells[*taken]));
            *taken += 1;
            tail.push(l);
            Ok(l)
        }
        ElemRebuild::Value(name, representation, fields) => {
            let mut fs = Vec::with_capacity(fields.len());
            for f in fields {
                let l = func.alloc_inst(Inst::Load(cells[*taken]));
                *taken += 1;
                tail.push(l);
                fs.push((*f, l));
            }
            let arena = func.rep_arena.as_ref().ok_or_else(|| {
                CompilerError::new_no_location(format!(
                    "bound element `{name}` has no owned representation arena"
                ))
            })?;
            if arena.try_get(*representation).is_none() {
                return Err(CompilerError::new_no_location(format!(
                    "bound element `{name}` carries a foreign value representation plan"
                )));
            }
            let packed = func.alloc_inst(Inst::MakeValueStruct {
                name: *name,
                representation: *representation,
                form: ValueCtorForm::Leaves,
                fields: fs,
            });
            tail.push(packed);
            Ok(packed)
        }
        ElemRebuild::Tuple(parts) => {
            let mut vs = Vec::with_capacity(parts.len());
            for p in parts {
                vs.push(emit_elem_rebuild(func, p, cells, taken, tail)?);
            }
            let packed = func.alloc_inst(Inst::MakeTuple(vs));
            tail.push(packed);
            Ok(packed)
        }
    }
}
