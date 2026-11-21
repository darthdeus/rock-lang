use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::bound::ResolvedBoundNext;
use skull_mir::concrete::ConcretePlans;
use skull_mir::lower::{RepArena, RepId, RepLayout, TupleLayout};
use skull_mir::mir::*;

pub fn resolve_standalone_bound_calls(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
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
        let split = mf.bound_next_split.contains(&id);
        // A bounded witness's own demand needs its subtree.
        if let ResolvedBoundNext::Concrete { sub, .. } = leaf {
            if !sub.is_empty() {
                mf.resolved_bound_trees.insert(id, sub.clone());
            }
        }
        substitute_bound_call(
            mf,
            id,
            split,
            leaf,
            scx,
            ws,
            concrete_plans,
            "in this instantiation",
        )?;
    }
    for nf in &mut mf.nested_fns {
        resolve_standalone_bound_calls(ws, scx, nf, incoming, concrete_plans)?;
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
    if !skull_mir::lower::requires_concrete_body_plan(ws, &owner) {
        return Err(CompilerError::new_no_location(
            "internal: a bound witness resolved to an opaque boundary without a concrete result shape",
        ));
    }
    let signature = &concrete_plans
        .get(&owner)
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "internal: concrete body plan is missing for bound witness `{}`",
                owner.func.func.into_raw().into_u32()
            ))
        })?
        .signature;
    let shape = abi
        .bound_next_layout(signature.result)
        .map_err(|error| CompilerError::new_no_location(error.message))?;
    if !owner.args.is_empty() {
        func.spec_bindings.insert(call, owner);
    }
    adapt_bound_next_result(func, call, already_split, scx.rep_arena(), shape)?;
    func.call_abi_plans.insert(call, abi);
    Ok(())
}

fn can_rebuild(arena: &RepArena, slot: RepId) -> bool {
    let plan = arena.get(slot);
    match plan.layout() {
        RepLayout::Opaque => true,
        RepLayout::Leaves(_) => plan.plain_leaf_fields().is_some(),
        RepLayout::Tuple(tuple) => {
            !tuple.nullable && tuple.slots.iter().all(|&slot| can_rebuild(arena, slot))
        }
    }
}

fn adapt_bound_next_result(
    func: &mut MirFunc,
    call: Idx<Inst>,
    already_split: bool,
    arena: &RepArena,
    shape: &TupleLayout,
) -> Result<(), CompilerError> {
    let Inst::CallMulti { dests, .. } = &func.insts[call] else {
        return Ok(());
    };
    let slot_count = shape.slot_count(arena);
    if already_split {
        if dests.len() == slot_count {
            return Ok(());
        }
        return Err(CompilerError::new_no_location(format!(
            "`{}`: the loop element was split into {} destinations but `next` returns \
             {slot_count} values at this specialization -- the two read the same instantiation \
             and must agree, or a leaf's value lands in another leaf's cell",
            func.name,
            dests.len()
        )));
    }
    // A one-leaf element has the same arity before and after splitting.
    let element = &shape.slots[1..];
    if !element.iter().any(|&slot| arena.get(slot).splats(arena)) {
        return Ok(());
    }
    if !element.iter().all(|&slot| can_rebuild(arena, slot)) {
        return Err(CompilerError::new_no_location(format!(
            "`{}` returns {slot_count} values at this specialization but its element \
             cannot be rebuilt from leaves (a nullable `nobox` element, whose \
             presence leaf pairs with no field).",
            func.name
        )));
    }
    let leaf_count: usize = element
        .iter()
        .map(|&slot| arena.get(slot).lanes(arena))
        .sum();
    let (ok_cell, elem_cell) = (dests[0], dests[1]);
    if leaf_count + 1 != slot_count {
        return Err(CompilerError::new_no_location(format!(
            "`{}`: `next` returns {} values but its element has {} leaves -- the \
             rebuild is positional over those leaves, so repacking a mismatched \
             pair would write one leaf's value into another field",
            func.name, slot_count, leaf_count
        )));
    }

    // The call keeps `ok` and gains one fresh cell per leaf; the element cell is
    // no longer a destination, it is what the repack stores into.
    let leaf_cells: Vec<Idx<Cell>> = (0..leaf_count).map(|_| func.new_cell(None)).collect();
    let mut new_dests = vec![ok_cell];
    new_dests.extend(leaf_cells.iter().copied());
    if let Inst::CallMulti { dests, .. } = &mut func.insts[call] {
        *dests = new_dests;
    }

    let mut tail: Vec<Idx<Inst>> = Vec::new();
    let mut cells = leaf_cells.into_iter();
    let mut repack = Repack {
        func,
        arena,
        cells: &mut cells,
        tail: &mut tail,
    };
    let packed = match element {
        [slot] => repack.slot(*slot)?,
        slots => repack.tuple(slots)?,
    };
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

struct Repack<'a> {
    func: &'a mut MirFunc,
    arena: &'a RepArena,
    cells: &'a mut std::vec::IntoIter<Idx<Cell>>,
    tail: &'a mut Vec<Idx<Inst>>,
}

impl Repack<'_> {
    fn emit(&mut self, inst: Inst) -> Idx<Inst> {
        let id = self.func.alloc_inst(inst);
        self.tail.push(id);
        id
    }

    fn leaf(&mut self) -> Idx<Inst> {
        let cell = self.cells.next().expect("checked repack lane count");
        self.emit(Inst::Load(cell))
    }

    fn tuple(&mut self, slots: &[RepId]) -> Result<Idx<Inst>, CompilerError> {
        let values = slots
            .iter()
            .map(|&slot| self.slot(slot))
            .collect::<Result<_, _>>()?;
        Ok(self.emit(Inst::MakeTuple(values)))
    }

    fn slot(&mut self, slot: RepId) -> Result<Idx<Inst>, CompilerError> {
        let plan = self.arena.get(slot);
        match plan.layout() {
            RepLayout::Opaque => Ok(self.leaf()),
            RepLayout::Tuple(tuple) => self.tuple(&tuple.slots),
            RepLayout::Leaves(_) => {
                let (name, fields) = plan.plain_leaf_fields().expect("checked repack shape");
                let fields = fields.map(|name| (name, self.leaf())).collect();
                let arena = self.func.rep_arena.as_ref().ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "bound element `{name}` has no owned representation arena"
                    ))
                })?;
                if arena.try_get(slot).is_none() {
                    return Err(CompilerError::new_no_location(format!(
                        "bound element `{name}` carries a foreign value representation plan"
                    )));
                }
                Ok(self.emit(Inst::MakeValueStruct {
                    name,
                    representation: slot,
                    form: ValueCtorForm::Leaves,
                    fields,
                }))
            }
        }
    }
}
