//! Lower suspension to a state-machine step and retain values live across a yield.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::detached::detached_region_blocks;
use crate::visit::{raw, term_value, top_level_blocks};
use comfy_la_arena::Idx;
use skull_mir::cfg::successors;
use skull_mir::code_identity::LambdaSiteId;
use skull_mir::mir::*;

pub const STATE: &str = "__state";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoroutineFramePlan {
    frame_cells: HashSet<Idx<Cell>>,
    frame_slots: HashMap<Idx<Cell>, u32>,
    spill_values: Vec<Idx<Inst>>,
    layout_hash: i64,
}

impl CoroutineFramePlan {
    fn new(
        frame_cells: HashSet<Idx<Cell>>,
        frame_slots: HashMap<Idx<Cell>, u32>,
        spill_values: Vec<Idx<Inst>>,
        layout_hash: i64,
    ) -> Self {
        Self {
            frame_cells,
            frame_slots,
            spill_values,
            layout_hash,
        }
    }

    pub fn fields(&self) -> &HashSet<Idx<Cell>> {
        &self.frame_cells
    }

    pub fn slots(&self) -> &HashMap<Idx<Cell>, u32> {
        &self.frame_slots
    }

    pub fn slot(&self, cell: Idx<Cell>) -> Option<u32> {
        self.frame_slots.get(&cell).copied()
    }

    pub fn spill_values(&self) -> &[Idx<Inst>] {
        &self.spill_values
    }

    pub fn layout_hash(&self) -> i64 {
        self.layout_hash
    }
}

pub struct CoroLowered {
    pub step: MirFunc,
    pub s_cell: Idx<Cell>,
    pub frame_plan: CoroutineFramePlan,
    pub params: Vec<Idx<Cell>>,
    pub needs_protected_resume: bool,
    /// Lifecycle hooks are emitted as ordinary bodies receiving the frame.
    pub hooks: Vec<(CoroHookKind, Idx<Block>)>,
    /// Closure conversion uses the same ownership as frame planning.
    pub step_tree: crate::scope::ScopeTree,
    pub step_own: crate::scope::Ownership,
}

/// FNV-1a 32 over the canonical layout string. 32-bit so the constant is
/// exactly representable in a Lua number.
fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn emit(f: &mut MirFunc, bid: Idx<Block>, inst: Inst) -> Idx<Inst> {
    let id = f.alloc_inst(inst);
    f.blocks[bid].insts.push(id);
    id
}

fn state_store(
    step: &mut MirFunc,
    block: Idx<Block>,
    state: Idx<Cell>,
    field: ustr::Ustr,
    value: Idx<Inst>,
) {
    let base = emit(step, block, Inst::Load(state));
    emit(step, block, Inst::StoreField { base, field, value });
}

fn stamp_state(
    step: &mut MirFunc,
    block: Idx<Block>,
    state: Idx<Cell>,
    next: i64,
    values: Vec<Idx<Inst>>,
) -> Idx<Block> {
    // A coroutine that killed itself must stay killed at yield or return.
    let base = emit(step, block, Inst::Load(state));
    let current = emit(
        step,
        block,
        Inst::Field {
            nonthrowing: false,
            base,
            field: ustr::Ustr::from(STATE),
        },
    );
    let killed = emit(step, block, Inst::Const(Lit::Int(-1)));
    let already_terminal = emit(step, block, Inst::Eq(current, killed));
    let killed_bb = step.new_block();
    let stamp_bb = step.new_block();
    step.blocks[block].term = Term::Branch {
        cond: already_terminal,
        then_bb: killed_bb,
        else_bb: stamp_bb,
    };
    let next = emit(step, stamp_bb, Inst::Const(Lit::Int(next)));
    state_store(step, stamp_bb, state, ustr::Ustr::from(STATE), next);
    step.blocks[stamp_bb].term = Term::Return(values);
    killed_bb
}

fn vfield(v: Idx<Inst>) -> ustr::Ustr {
    ustr::Ustr::from(format!("v{}", raw(v)).as_str())
}

/// Spill values live across a suspension. Rematerialize constants on the
/// other side, where untyped nil needs no physical frame field.
fn spill_cross_block_values(
    step: &mut MirFunc,
    s_cell: Idx<Cell>,
    block_ids: &[Idx<Block>],
    resume_blocks: &[Idx<Block>],
) -> Vec<Idx<Inst>> {
    let mut def_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for &bid in block_ids {
        for &id in &step.blocks[bid].insts {
            if step.insts[id].produces_value() {
                def_block.insert(id, bid);
            }
        }
    }
    let mut cross_block_values: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in block_ids {
        let mut used: Vec<Idx<Inst>> = Vec::new();
        for &id in &step.blocks[bid].insts {
            step.insts[id].uses_each(&mut |v| used.push(v));
        }
        used.extend(term_value(&step.blocks[bid].term));
        for v in used {
            if def_block.get(&v).copied() != Some(bid) {
                cross_block_values.insert(v);
            }
        }
    }
    let live_values = values_live_at_resume_blocks(step, block_ids, resume_blocks);
    let persistent_values = cross_block_values
        .intersection(&live_values)
        .copied()
        .collect::<HashSet<_>>();
    let rematerialized = persistent_values
        .iter()
        .copied()
        .filter(|value| matches!(step.insts[*value], Inst::Const(_)))
        .collect::<HashSet<_>>();
    let frame_values = persistent_values
        .difference(&rematerialized)
        .copied()
        .collect::<HashSet<_>>();
    if persistent_values.is_empty() {
        return Vec::new();
    }

    for &bid in block_ids {
        let mut used_here: Vec<Idx<Inst>> = Vec::new();
        {
            let mut seen = HashSet::default();
            for &id in &step.blocks[bid].insts {
                step.insts[id].uses_each(&mut |v| {
                    if persistent_values.contains(&v)
                        && def_block.get(&v).copied() != Some(bid)
                        && seen.insert(v)
                    {
                        used_here.push(v);
                    }
                });
            }
            if let Some(v) = term_value(&step.blocks[bid].term) {
                if persistent_values.contains(&v)
                    && def_block.get(&v).copied() != Some(bid)
                    && seen.insert(v)
                {
                    used_here.push(v);
                }
            }
        }
        used_here.sort_by_key(|v| raw(*v));

        let old: Vec<Idx<Inst>> = std::mem::take(&mut step.blocks[bid].insts);
        let mut reload: HashMap<Idx<Inst>, Idx<Inst>> = HashMap::default();
        let mut new_list: Vec<Idx<Inst>> = Vec::new();
        for v in used_here {
            let rv = if rematerialized.contains(&v) {
                step.alloc_inst(step.insts[v].clone())
            } else {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                new_list.push(sv);
                step.alloc_inst(Inst::Field {
                    nonthrowing: false,
                    base: sv,
                    field: vfield(v),
                })
            };
            new_list.push(rv);
            reload.insert(v, rv);
        }
        let m = |x: Idx<Inst>| reload.get(&x).copied().unwrap_or(x);
        for id in old {
            step.insts[id].remap_uses(&m);
            new_list.push(id);
            if frame_values.contains(&id) && def_block.get(&id) == Some(&bid) {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                let st = step.alloc_inst(Inst::StoreField {
                    base: sv,
                    field: vfield(id),
                    value: id,
                });
                new_list.push(sv);
                new_list.push(st);
            }
        }
        step.blocks[bid].insts = new_list;
        let mut term = step.blocks[bid].term.clone();
        term.remap_uses(&m);
        step.blocks[bid].term = term;
    }
    let mut values: Vec<Idx<Inst>> = frame_values.into_iter().collect();
    values.sort_by_key(|value| raw(*value));
    values
}

fn cells_live_at_yield_resumes(func: &MirFunc, blocks: &[Idx<Block>]) -> HashSet<Idx<Cell>> {
    let mut block_use: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    let mut block_def: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    for &block in blocks {
        let mut uses = HashSet::default();
        let mut defs = HashSet::default();
        for &id in &func.blocks[block].insts {
            let inst = &func.insts[id];
            if let Inst::Load(cell) | Inst::MapNext { key_cell: cell, .. } = inst
                && !defs.contains(cell)
            {
                uses.insert(*cell);
            }
            inst.cell_writes_each(&mut |cell| {
                defs.insert(cell);
            });
        }
        block_use.insert(block, uses);
        block_def.insert(block, defs);
    }

    let live_in = live_in(func, blocks, &block_use, &block_def);

    let mut live = HashSet::default();
    for &block in blocks {
        if let Term::Yield { resume, .. } = func.blocks[block].term {
            live.extend(live_in[&resume].iter().copied());
        }
    }
    live
}

fn live_in<K: Copy + Eq + std::hash::Hash>(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    block_use: &HashMap<Idx<Block>, HashSet<K>>,
    block_def: &HashMap<Idx<Block>, HashSet<K>>,
) -> HashMap<Idx<Block>, HashSet<K>> {
    let mut live_in: HashMap<Idx<Block>, HashSet<K>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut out = HashSet::default();
            successors(&func.blocks[block].term, |succ| {
                if let Some(next) = live_in.get(&succ) {
                    out.extend(next.iter().copied());
                }
            });
            let mut input = block_use[&block].clone();
            input.extend(out.difference(&block_def[&block]).copied());
            let previous = live_in.get_mut(&block).unwrap();
            if *previous != input {
                *previous = input;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_in
}

/// A defer's scope-exit splice shares closure bodies with the detached copy.
/// Its locals must therefore use the same capture representation.
fn detached_uses(
    func: &MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) -> (HashSet<Idx<Cell>>, HashSet<LambdaSiteId>) {
    let mut cells = HashSet::default();
    let mut sites = HashSet::default();
    for block in entries
        .into_iter()
        .flat_map(|entry| detached_region_blocks(func, entry))
    {
        for &id in &func.blocks[block].insts {
            let inst = &func.insts[id];
            inst.cells_each(&mut |cell| {
                cells.insert(cell);
            });
            if matches!(inst, Inst::Lambda { .. })
                && let Some(site) = lambda_site(func, id)
            {
                sites.insert(site);
            }
        }
    }
    (cells, sites)
}

fn lambda_site(func: &MirFunc, lambda: Idx<Inst>) -> Option<LambdaSiteId> {
    func.inst_spans
        .get(lambda)
        .copied()
        .and_then(LambdaSiteId::from_span)
}

fn values_live_at_resume_blocks(
    func: &MirFunc,
    blocks: &[Idx<Block>],
    resume_blocks: &[Idx<Block>],
) -> HashSet<Idx<Inst>> {
    let mut block_use: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = HashMap::default();
    let mut block_def: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = HashMap::default();
    for &block in blocks {
        let mut uses = HashSet::default();
        let mut defs = HashSet::default();
        for &id in &func.blocks[block].insts {
            func.insts[id].uses_each(&mut |value| {
                if !defs.contains(&value) {
                    uses.insert(value);
                }
            });
            if func.insts[id].produces_value() {
                defs.insert(id);
            }
        }
        func.blocks[block].term.uses_each(&mut |value| {
            if !defs.contains(&value) {
                uses.insert(value);
            }
        });
        block_use.insert(block, uses);
        block_def.insert(block, defs);
    }

    let live_in = live_in(func, blocks, &block_use, &block_def);

    let mut live = HashSet::default();
    for &resume in resume_blocks {
        live.extend(live_in[&resume].iter().copied());
    }
    live
}

pub fn transform_coroutine(
    func: &MirFunc,
    channel: Option<&skull_mir::lower::EnumFlatShape>,
    has_return_value: bool,
) -> CoroLowered {
    let frame_slot_channel = channel.is_some();
    let mut step = func.clone();
    let resume_arg = func.resume_arg.expect("co fn must carry a resume-arg cell");
    let params = func.params.clone();
    let s_cell = step.new_cell(Some(ustr::Ustr::from("s")));
    step.cells[s_cell].capture_identity =
        Some(skull_mir::code_identity::CaptureIdentity::Generated {
            role: ustr::Ustr::from("coroutine-frame"),
        });

    // A closure has its own returns and lexical scope.
    let orig_blocks: Vec<Idx<Block>> = top_level_blocks(&step);

    // Hooks run outside the step, so their bodies need separate control flow.
    let mut hooks: Vec<(CoroHookKind, Idx<Block>)> = Vec::new();
    let mut hook_lambdas: HashSet<Idx<Inst>> = HashSet::default();
    for &bid in &orig_blocks {
        for &id in &step.blocks[bid].insts {
            if let Inst::CoroHook { kind, lambda } = step.insts[id] {
                let Inst::Lambda { entry, .. } = step.insts[lambda] else {
                    panic!("co-fn hook {} must point at a lambda", kind.slot());
                };
                hooks.push((kind, entry));
                hook_lambdas.insert(lambda);
            }
        }
    }
    if !hooks.is_empty() {
        for &bid in &orig_blocks {
            step.blocks[bid].insts.retain(|&id| {
                !matches!(step.insts[id], Inst::CoroHook { .. }) && !hook_lambdas.contains(&id)
            });
        }
    }

    let needs_protected_resume = !step.coro_defers.is_empty()
        || hooks
            .iter()
            .any(|(kind, _)| *kind == CoroHookKind::OnKilled)
        || (hooks
            .iter()
            .any(|(kind, _)| *kind == CoroHookKind::OnResume)
            && orig_blocks.iter().any(|&bid| {
                step.blocks[bid]
                    .insts
                    .iter()
                    .any(|&id| matches!(step.insts[id], Inst::CoroSetInner(_)))
            }));

    // Analyze ownership while the yield edges still connect the root CFG.
    let tree = crate::scope::ScopeTree::build(&step);
    let own = tree.analyze(&step);
    let frame_cells: HashSet<Idx<Cell>> = {
        let root = crate::scope::ROOT;
        let root_owned = |cell: &Idx<Cell>| own.owner.get(cell).is_some_and(|&owner| owner == root);
        let (detached_cells, detached_sites) = detached_uses(
            &step,
            step.coro_defers
                .iter()
                .map(|(_, entry)| *entry)
                .chain(hooks.iter().map(|(_, entry)| *entry)),
        );
        let captured_by_escaping_lambda = own
            .captures
            .iter()
            .enumerate()
            .filter(|(scope, _)| {
                tree.scopes[*scope].lambda.is_none_or(|lambda| {
                    !lambda_site(&step, lambda).is_some_and(|site| detached_sites.contains(&site))
                })
            })
            .flat_map(|(_, cells)| cells.iter().copied())
            .collect::<HashSet<_>>();
        let mut fields = cells_live_at_yield_resumes(&step, &orig_blocks);
        fields.extend(params.iter().copied());
        fields.extend(captured_by_escaping_lambda);
        fields.extend(detached_cells);
        fields.retain(|cell| *cell != s_cell && *cell != resume_arg && root_owned(cell));
        fields
    };
    // A lambda's private cell IDs must not shift its parent's frame slots.
    let frame_slots = dense_slots(frame_cells.iter().copied());

    for &bid in &orig_blocks {
        let has = step.blocks[bid]
            .insts
            .iter()
            .any(|&i| matches!(step.insts[i], Inst::CoroSetInner(_)));
        if !has {
            continue;
        }
        let old: Vec<Idx<Inst>> = std::mem::take(&mut step.blocks[bid].insts);
        for id in old {
            if let Inst::CoroSetInner(x) = step.insts[id] {
                let sv = step.alloc_inst(Inst::Load(s_cell));
                step.blocks[bid].insts.push(sv);
                let store = step.alloc_inst(Inst::StoreField {
                    base: sv,
                    field: ustr::Ustr::from("__inner"),
                    value: x,
                });
                step.blocks[bid].insts.push(store);
            } else {
                step.blocks[bid].insts.push(id);
            }
        }
    }

    let suspension_resumes: Vec<Idx<Block>> = orig_blocks
        .iter()
        .filter_map(|&block| match step.blocks[block].term {
            Term::Yield { resume, .. } => Some(resume),
            _ => None,
        })
        .collect();
    let mut resume_of_state: Vec<Idx<Block>> = Vec::new();
    let mut state_tags: Vec<String> = Vec::new();
    let mut yield_blocks: HashSet<Idx<Block>> = HashSet::default();
    for &bid in &orig_blocks {
        if let Term::Yield { values, resume } = &step.blocks[bid].term {
            let values = values.clone();
            let resume = *resume;
            let k = resume_of_state.len() as i64 + 1;
            resume_of_state.push(resume);
            let tag = values
                .first()
                .and_then(|&v| match &step.insts[v] {
                    Inst::Const(Lit::Str(s)) => Some(s.as_str().to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "dyn".to_string());
            state_tags.push(tag);
            yield_blocks.insert(bid);
            // Reuse frame slots for flat enum yields to avoid per-yield allocation.
            if frame_slot_channel {
                for (i, &v) in values.iter().enumerate() {
                    state_store(&mut step, bid, s_cell, skull_mir::lower::yield_slot(i), v);
                }
            }
            let returned = if frame_slot_channel {
                Vec::new()
            } else {
                values
            };
            let killed_bb = stamp_state(&mut step, bid, s_cell, k, returned);
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }
    let num_states = resume_of_state.len();

    for &bid in &orig_blocks {
        if yield_blocks.contains(&bid) {
            continue;
        }
        let Term::Return(values) = &step.blocks[bid].term else {
            continue;
        };
        let result = has_return_value.then(|| values.first().copied()).flatten();
        let field = ustr::Ustr::from("__return");
        if let Some(value) = result {
            state_store(&mut step, bid, s_cell, field, value);
        }
        let killed_bb = stamp_state(&mut step, bid, s_cell, -3, Vec::new());
        if result.is_some() {
            let nil = emit(&mut step, killed_bb, Inst::Const(Lit::Nil));
            state_store(&mut step, killed_bb, s_cell, field, nil);
        }
        step.blocks[killed_bb].term = Term::Return(Vec::new());
    }

    let spill_values =
        spill_cross_block_values(&mut step, s_cell, &orig_blocks, &suspension_resumes);

    // The layout stamp lets compatible body edits keep live coroutines.
    // It describes frame fields and resume states, excluding body logic.
    let mut canon = String::new();
    {
        use std::fmt::Write as _;
        for (id, cell) in step.cells.iter() {
            if let Some(&slot) = frame_slots.get(&id) {
                let _ = write!(
                    canon,
                    "c{}:{};",
                    slot,
                    cell.name
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_default()
                );
            }
        }
        for &value in &spill_values {
            let _ = write!(canon, "{};", vfield(value));
        }
        for (i, t) in state_tags.iter().enumerate() {
            let _ = write!(canon, "s{i}:{t};");
        }
        match channel {
            Some(ch) => {
                let name = bone_core::modules::artifact_name(ch.name().as_str());
                let _ = write!(canon, "ch:{}:{}", name, ch.tuple_width());
                for rf in ch.record_fields() {
                    let _ = write!(canon, ":{rf}");
                }
            }
            None => canon.push_str("ch:-"),
        }
    }
    let layout_hash = fnv1a32(&canon) as i64;

    let entry = step.new_block();
    let frame = emit(&mut step, entry, Inst::Load(s_cell));
    let saved_state = emit(
        &mut step,
        entry,
        Inst::Field {
            nonthrowing: false,
            base: frame,
            field: ustr::Ustr::from(STATE),
        },
    );
    // A raised step stays terminal without a protected call. A successful
    // yield or return replaces this marker; explicit kill writes -1 instead.
    let running = emit(&mut step, entry, Inst::Const(Lit::Int(-2)));
    emit(
        &mut step,
        entry,
        Inst::StoreField {
            base: frame,
            field: ustr::Ustr::from(STATE),
            value: running,
        },
    );
    let hook = emit(
        &mut step,
        entry,
        Inst::Field {
            nonthrowing: false,
            base: frame,
            field: ustr::Ustr::from("__on_resume"),
        },
    );
    let nil = emit(&mut step, entry, Inst::Const(Lit::Nil));
    let has_hook = emit(&mut step, entry, Inst::NotEq(hook, nil));
    let call_hook = step.new_block();
    emit(
        &mut step,
        call_hook,
        Inst::CallValue {
            callee: hook,
            args: vec![frame],
        },
    );
    let orig_entry = step.entry;
    if num_states == 0 {
        let d = step.new_block();
        step.blocks[d].term = Term::Goto(orig_entry);
        step.entry = d;
    } else {
        let check_blocks: Vec<Idx<Block>> = (0..num_states).map(|_| step.new_block()).collect();
        for k in 0..num_states {
            let bid = check_blocks[k];
            let kc = emit(&mut step, bid, Inst::Const(Lit::Int(k as i64 + 1)));
            let eq = emit(&mut step, bid, Inst::Eq(saved_state, kc));
            let else_bb = if k + 1 < num_states {
                check_blocks[k + 1]
            } else {
                orig_entry
            };
            step.blocks[bid].term = Term::Branch {
                cond: eq,
                then_bb: resume_of_state[k],
                else_bb,
            };
        }
        step.entry = check_blocks[0];
    }

    // Reject a changed frame layout before resuming into the new body.
    {
        let dispatch = step.entry;
        let chk = step.new_block();
        let sv = emit(&mut step, chk, Inst::Load(s_cell));
        let lay = emit(
            &mut step,
            chk,
            Inst::Field {
                nonthrowing: false,
                base: sv,
                field: ustr::Ustr::from("__layout"),
            },
        );
        let kc = emit(&mut step, chk, Inst::Const(Lit::Int(layout_hash)));
        let ne = emit(&mut step, chk, Inst::NotEq(lay, kc));
        let raise = step.new_block();
        let kill = emit(
            &mut step,
            raise,
            Inst::Global(ustr::Ustr::from("__rock_coroutine_kill")),
        );
        emit(
            &mut step,
            raise,
            Inst::CallValue {
                callee: kill,
                args: vec![frame],
            },
        );
        let err_fn = emit(&mut step, raise, Inst::Global(ustr::Ustr::from("error")));
        let msg = emit(
            &mut step,
            raise,
            Inst::Const(Lit::Str(ustr::Ustr::from(
                format!(
                    "coroutine killed: frame layout changed by hot reload ({})",
                    bone_core::modules::display_name(func.name.as_str())
                )
                .as_str(),
            ))),
        );
        emit(
            &mut step,
            raise,
            Inst::CallValue {
                callee: err_fn,
                args: vec![msg],
            },
        );
        // Unreachable (error() raised); a terminator is still required.
        step.blocks[raise].term = Term::Goto(dispatch);
        step.blocks[chk].term = Term::Branch {
            cond: ne,
            then_bb: raise,
            else_bb: dispatch,
        };
        step.entry = chk;
    }

    step.blocks[entry].term = Term::Branch {
        cond: has_hook,
        then_bb: call_hook,
        else_bb: step.entry,
    };
    step.blocks[call_hook].term = Term::Goto(step.entry);
    step.entry = entry;

    step.name = ustr::Ustr::from(format!("{}__step", func.name).as_str());
    step.params = vec![s_cell, resume_arg];
    step.is_coroutine = false;
    step.resume_arg = None;

    CoroLowered {
        step,
        s_cell,
        frame_plan: CoroutineFramePlan::new(frame_cells, frame_slots, spill_values, layout_hash),
        params,
        needs_protected_resume,
        hooks,
        step_tree: tree,
        step_own: own,
    }
}

fn dense_slots(cells: impl Iterator<Item = Idx<Cell>>) -> HashMap<Idx<Cell>, u32> {
    let mut v: Vec<Idx<Cell>> = cells.collect();
    v.sort_by_key(|c| raw(*c));
    v.into_iter()
        .enumerate()
        .map(|(i, c)| (c, i as u32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower;

    #[test]
    fn coroutine_demotes_a_local_confined_to_one_resume_segment() {
        let func = lower(
            "co fn f(x: int) -> Coroutine<int> {\n\
                 yield 1\n\
                 let after = x\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        assert!(
            lowered
                .frame_plan
                .fields()
                .iter()
                .all(|&cell| lowered.step.cells[cell].name != Some(ustr::Ustr::from("after"))),
            "a local used only after the last yield must stay out of the frame:\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_rematerializes_cross_block_constants_but_spills_a_live_global() {
        let mut func = MirFunc::new(ustr::Ustr::from("f"));
        func.is_coroutine = true;
        func.resume_arg = Some(func.new_cell(Some(ustr::Ustr::from("__resume_arg"))));
        let resume = func.new_block();
        let then_bb = func.new_block();
        let else_bb = func.new_block();
        let global = func.emit(func.entry, Inst::Global(ustr::Ustr::from("SELF")));
        let constant = func.emit(func.entry, Inst::Const(Lit::Int(7)));
        func.set_term(
            func.entry,
            Term::Yield {
                values: vec![constant],
                resume,
            },
        );
        func.emit(
            resume,
            Inst::StoreGlobal {
                name: ustr::Ustr::from("SAVED"),
                value: global,
            },
        );
        let condition = func.emit(resume, Inst::IntLt(constant, constant));
        func.set_term(
            resume,
            Term::Branch {
                cond: condition,
                then_bb,
                else_bb,
            },
        );
        func.set_term(then_bb, Term::Return(Vec::new()));
        func.set_term(else_bb, Term::Return(Vec::new()));
        let lowered = transform_coroutine(&func, None, false);
        let spills = lowered.frame_plan.spill_values();
        assert!(
            spills
                .iter()
                .any(|value| matches!(lowered.step.insts[*value], Inst::Global(name) if name.as_str() == "SELF")),
            "the evaluated global handle must retain its pre-branch identity:\n{}",
            lowered.step
        );
        assert!(
            spills
                .iter()
                .all(|value| !matches!(lowered.step.insts[*value], Inst::Const(_))),
            "constants must not acquire frame fields: {spills:?}\n{}",
            lowered.step
        );
    }
}
