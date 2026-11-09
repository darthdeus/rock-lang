//! Coroutine transform: a `co fn` (lowered to a cell CFG with `Term::Yield`)
//! becomes an ordinary stackless state-machine `__step` function plus a
//! constructor. No LuaJIT
//! coroutines.
//!
//! The cell model SIMPLIFIES this vs the old SSA backend: **only cells cross an
//! await** (values are single-block), so there is NO value spill/reload and NO
//! "block-param value crossing a suspend" case. The frame (state-table fields)
//! is just cells — rendered as `s.<field>` by `skull-codegen-lua`'s `FrameCtx`,
//! so the transform doesn't even rewrite cell Load/Store; it only:
//!   1. turns each `Term::Yield { value, resume }` block into `s.__state = k;
//!      return value` (the `resume` block becomes a dispatch target),
//!   2. turns a normal `return v` into `s.__return = v; s.__state = -3; return`
//!      (-3 = completed normally, distinct from killed -1 / errored -2 so
//!      `completed()` can tell a natural finish from a kill; every runtime
//!      guard tests `< 0`, never an exact terminal). Both stamps (yield and
//!      completion) are guarded on `state >= 0`: a body that killed its own
//!      handle mid-run stays killed,
//!   3. rewrites `CoroSetInner(x)` (the `await` pseudo-op) to `s.__inner = x`,
//!   4. prepends a dispatch entry that branches on `s.__state` to the resume
//!      block for each state (falling through to the original entry for state 0).
//!
//! Frame = cells live at a resume point, plus parameters and cells retained by a
//! closure or detached lifecycle body. The constructor seeds only the declared
//! param fields.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::closure::FrameInfo;
use crate::detached::detached_region_blocks;
use crate::visit::{raw, remap_inst, remap_term, term_succs, term_value, top_level_blocks};
use comfy_la_arena::Idx;
use skull_mir::code_identity::LambdaSiteId;
use skull_mir::mir::*;

pub const STATE: &str = "__state";

/// The physical frame layout chosen for one transformed coroutine body.
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
    /// Declared param cells (constructor seeds a state-table field for each).
    pub params: Vec<Idx<Cell>>,
    pub num_states: usize,
    /// `on_killed`/`on_resume`/`on_hot_reload` hooks peeled to the constructor:
    /// `(kind, detached body entry)`. The body is emitted as its own ordinary
    /// `MirFunc`, just like a closure sibling.
    pub hooks: Vec<(CoroHookKind, Idx<Block>)>,
    /// The scope tree + ownership built (once) for frame membership, threaded to
    /// `emit_named_framed` so the step's hoist plan reuses them instead of
    /// rebuilding on the post-rewrite CFG (they are stable across the rewrite).
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

/// Append `inst` to block `bid` of `f`, returning its id.
fn emit(f: &mut MirFunc, bid: Idx<Block>, inst: Inst) -> Idx<Inst> {
    let id = f.alloc_inst(inst);
    f.blocks[bid].insts.push(id);
    id
}

fn vfield(v: Idx<Inst>) -> ustr::Ustr {
    ustr::Ustr::from(format!("v{}", raw(v)).as_str())
}

/// Spill a CROSS-BLOCK value that is also live across a suspension to the state
/// table at its def and reload it at each using block. Values crossing ordinary
/// branches stay as step locals; only a value live into a yield's resume block
/// needs persistence. Constants are rematerialized at each use block: they have
/// no evaluation-time identity to preserve, and polymorphic null has no
/// standalone physical type for a frame field. Returns the sorted spilled
/// values; their `v<N>` fields are part of the frame layout stamp.
fn spill_cross_block_values(
    step: &mut MirFunc,
    s_cell: Idx<Cell>,
    block_ids: &[Idx<Block>],
    resume_blocks: &[Idx<Block>],
) -> Vec<Idx<Inst>> {
    // def block of every value.
    let mut def_block: HashMap<Idx<Inst>, Idx<Block>> = HashMap::default();
    for &bid in block_ids {
        for &id in &step.blocks[bid].insts {
            if step.insts[id].produces_value() {
                def_block.insert(id, bid);
            }
        }
    }
    // cross_block_values = values used in a block other than where defined.
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
        // Reloads needed in this block: frame values used here but defined
        // elsewhere. Sorted for deterministic output.
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
                    base: sv,
                    field: vfield(v),
                })
            };
            new_list.push(rv);
            reload.insert(v, rv);
        }
        let m = |x: Idx<Inst>| reload.get(&x).copied().unwrap_or(x);
        for id in old {
            remap_inst(&mut step.insts[id], &m);
            new_list.push(id);
            // Spill right after a frame value's def.
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
        remap_term(&mut term, &m);
        step.blocks[bid].term = term;
    }
    let mut values: Vec<Idx<Inst>> = frame_values.into_iter().collect();
    values.sort_by_key(|value| raw(*value));
    values
}

/// Cells needed on the far side of at least one suspension. Ordinary CFG
/// branches do not make a cell persistent; only a cell live into a yield's
/// resume block does. The dataflow is deliberately cell-only because MIR
/// values crossing blocks already use `spill_cross_block_values`.
fn cells_live_at_yield_resumes(func: &MirFunc, blocks: &[Idx<Block>]) -> HashSet<Idx<Cell>> {
    let mut block_use: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    let mut block_def: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = HashMap::default();
    for &block in blocks {
        let mut uses = HashSet::default();
        let mut defs = HashSet::default();
        for &id in &func.blocks[block].insts {
            match &func.insts[id] {
                Inst::Load(cell) => {
                    if !defs.contains(cell) {
                        uses.insert(*cell);
                    }
                }
                Inst::Store { cell, .. } => {
                    defs.insert(*cell);
                }
                Inst::CallMulti { dests, .. }
                | Inst::CallValueMulti { dests, .. }
                | Inst::CallBoundMulti { dests, .. } => {
                    defs.extend(dests.iter().copied());
                }
                Inst::MapNext {
                    key_cell, val_cell, ..
                } => {
                    if !defs.contains(key_cell) {
                        uses.insert(*key_cell);
                    }
                    defs.insert(*key_cell);
                    defs.insert(*val_cell);
                }
                _ => {}
            }
        }
        block_use.insert(block, uses);
        block_def.insert(block, defs);
    }

    let mut live_in: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    let mut live_out: HashMap<Idx<Block>, HashSet<Idx<Cell>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut out = HashSet::default();
            term_succs(&func.blocks[block].term, |succ| {
                if let Some(next) = live_in.get(&succ) {
                    out.extend(next.iter().copied());
                }
            });
            let mut input = block_use[&block].clone();
            input.extend(out.difference(&block_def[&block]).copied());
            if live_out
                .insert(block, out.clone())
                .is_some_and(|old| old != out)
            {
                changed = true;
            }
            if live_in
                .insert(block, input.clone())
                .is_some_and(|old| old != input)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut live = HashSet::default();
    for &block in blocks {
        if let Term::Yield { resume, .. } = func.blocks[block].term {
            live.extend(live_in[&resume].iter().copied());
        }
    }
    live
}

/// Cells used by detached defer/hook regions must remain in the shared frame;
/// those regions run independently of the current step segment.
fn cells_referenced_by_detached_regions(
    func: &MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) -> HashSet<Idx<Cell>> {
    let mut cells = HashSet::default();
    for entry in entries {
        for block in detached_region_blocks(func, entry) {
            for &id in &func.blocks[block].insts {
                func.insts[id].cells_each(&mut |cell| {
                    cells.insert(cell);
                });
            }
        }
    }
    cells
}

/// Source sites of the lambdas inside detached defer/hook regions.
///
/// A defer's scope-exit splice in the step is a second lowering of the same
/// body, so its lambdas share their sites, and thus their code identities and
/// hoisted bodies, with the detached copy. The detached copy keeps the
/// defer's own locals off the frame (boxed when a lambda writes them), so the
/// splice must capture those locals the same way: pinning them to the frame
/// for an escaping lambda would hand the shared body the frame where it
/// expects a box or a value. A defer body cannot yield, so its locals never
/// need the frame to survive a suspension.
fn detached_lambda_sites(
    func: &MirFunc,
    entries: impl IntoIterator<Item = Idx<Block>>,
) -> HashSet<LambdaSiteId> {
    let mut sites = HashSet::default();
    for entry in entries {
        for block in detached_region_blocks(func, entry) {
            for &id in &func.blocks[block].insts {
                if matches!(func.insts[id], Inst::Lambda { .. })
                    && let Some(site) = lambda_site(func, id)
                {
                    sites.insert(site);
                }
            }
        }
    }
    sites
}

fn lambda_site(func: &MirFunc, lambda: Idx<Inst>) -> Option<LambdaSiteId> {
    func.inst_spans
        .get(lambda)
        .copied()
        .and_then(LambdaSiteId::from_span)
}

/// Values live into resume blocks, used to narrow the cross-block spill
/// candidates to values that must survive a suspension rather than a branch.
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

    let mut live_in: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    let mut live_out: HashMap<Idx<Block>, HashSet<Idx<Inst>>> = blocks
        .iter()
        .copied()
        .map(|block| (block, HashSet::default()))
        .collect();
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut out = HashSet::default();
            term_succs(&func.blocks[block].term, |succ| {
                if let Some(next) = live_in.get(&succ) {
                    out.extend(next.iter().copied());
                }
            });
            let mut input = block_use[&block].clone();
            input.extend(out.difference(&block_def[&block]).copied());
            if live_out
                .insert(block, out.clone())
                .is_some_and(|old| old != out)
            {
                changed = true;
            }
            if live_in
                .insert(block, input.clone())
                .is_some_and(|old| old != input)
            {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut live = HashSet::default();
    for &resume in resume_blocks {
        live.extend(live_in[&resume].iter().copied());
    }
    live
}

/// Transform a lowered `co fn` into its `__step` state machine + frame info.
pub fn transform_coroutine(
    func: &MirFunc,
    channel: Option<&skull_mir::lower::YieldChannel>,
    has_return_value: bool,
) -> CoroLowered {
    transform_coroutine_with_nonescaping_lambdas(
        func,
        channel,
        has_return_value,
        &HashSet::default(),
    )
}

/// Transform a coroutine while excluding captures proven to be consumed by a
/// synchronous callback call from the persistent frame. The proof is supplied
/// by the backend that owns the callable ABI; an empty set preserves the
/// conservative frame pin for backends without that proof.
pub fn transform_coroutine_with_nonescaping_lambdas(
    func: &MirFunc,
    channel: Option<&skull_mir::lower::YieldChannel>,
    has_return_value: bool,
    nonescaping_lambdas: &HashSet<Idx<Inst>>,
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

    // Only the co-fn's TOP-LEVEL blocks are transformed — NOT closure sub-blocks
    // (which share the arena but are their own CFGs, reached via `Inst::Lambda`,
    // not terminator successors). A closure body must keep its own `return`s and
    // never get coroutine state machinery; it reads captured frame cells as
    // `s.<field>` purely via codegen's `FrameCtx`.
    let orig_blocks: Vec<Idx<Block>> = top_level_blocks(&step);

    // 0. Peel `on_killed`/`on_resume` hook registrations to the constructor so a
    // kill before the first resume still fires (the body would otherwise run the
    // registration only on first resume). Both the `CoroHook` effect and its
    // lambda `Inst` are removed from every block's inst list; the lambda stays in
    // the arena (its body is hoisted to a top-level `<name>__on_<hook>` fn).
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

    // 0b. Frame membership is liveness-based with ownership pins (P1):
    // root cells live at a resume point
    // become fields. Root cells captured by a live lambda or referenced by a
    // detached hook/defer stay fields even when the local CFG has no later use.
    // Built post-peel so hook lambdas are detached, pre-rewrite so `Yield`
    // resume edges still link the CFG.
    // The tree + ownership are built ONCE here (post-hook-peel, pre-yield-rewrite)
    // and threaded to emission through `CoroLowered` -- emission reuses them for
    // the step's hoist plan instead of rebuilding on the post-rewrite CFG. The
    // facts are stable across the yield rewrite: lambda sub-CFGs (which own every
    // hoisted cell) are untouched, and the rewrite only ADDS ROOT-region blocks +
    // `s`-field accesses, which the frame collapses to `s` anyway. Byte-identity
    // of emitted Lua is the standing oracle for that stability.
    let tree = crate::scope::ScopeTree::build(&step);
    let own = tree.analyze(&step);
    let frame_cells: HashSet<Idx<Cell>> = {
        let root = crate::scope::ROOT;
        let root_owned = |cell: &Idx<Cell>| own.owner.get(cell).is_some_and(|&owner| owner == root);
        let detached_sites = detached_lambda_sites(
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
                    !nonescaping_lambdas.contains(&lambda)
                        && !lambda_site(&step, lambda)
                            .is_some_and(|site| detached_sites.contains(&site))
                })
            })
            .flat_map(|(_, cells)| cells.iter().copied())
            .collect::<HashSet<_>>();
        let mut fields = cells_live_at_yield_resumes(&step, &orig_blocks);
        fields.extend(params.iter().copied());
        fields.extend(
            captured_by_escaping_lambda
                .into_iter()
                .filter(|cell| root_owned(cell)),
        );
        let detached_entries = step
            .coro_defers
            .iter()
            .map(|(_, entry)| *entry)
            .chain(hooks.iter().map(|(_, entry)| *entry));
        fields.extend(
            cells_referenced_by_detached_regions(&step, detached_entries)
                .into_iter()
                .filter(|cell| root_owned(cell)),
        );
        fields.retain(|cell| *cell != s_cell && *cell != resume_arg && root_owned(cell));
        fields
    };
    // Dense per-frame slots: frame cells numbered 0.. in raw-id order. Both the
    // emitted `s.c<slot>` field names and the layout canon key on the slot, so a
    // lambda-internal cell-count edit (which perturbs raw ids after the lambda)
    // leaves every frame slot fixed -- the P2 finding fix.
    let frame_slots = dense_slots(frame_cells.iter().copied());

    // 1. Rewrite `CoroSetInner(x)` -> `s.__inner = x` (rebuild affected blocks).
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

    // 2. Yields: `s.__state = k; return value`. Record (resume block) per state.
    // Per state, also record the yielded variant tag when statically known (a
    // `Const` tag leaf) -- a layout-stamp input that catches cross-variant yield
    // reordering, which cell/spill/state-count hashing alone cannot see.
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
            // Frame-slot channel (flat-enum yield): write the leaves to reused
            // `s.__y*` slots and return nothing (0 alloc per yield). The consumer
            // reads the slots at the concrete resume site. Otherwise the value(s)
            // multi-return directly (scalar / bare yield -- no box to remove).
            if frame_slot_channel {
                for (i, &v) in values.iter().enumerate() {
                    let sv = emit(&mut step, bid, Inst::Load(s_cell));
                    emit(
                        &mut step,
                        bid,
                        Inst::StoreField {
                            base: sv,
                            field: skull_mir::lower::yield_slot(i),
                            value: v,
                        },
                    );
                }
            }
            // The stamp is guarded on `state >= 0` (mirror the completion
            // stamp): a body that killed its own handle mid-run must stay
            // killed, not resurrect via a positive resume state. The killed
            // arm returns nothing -- a dead coroutine's resume yields nil.
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch {
                cond: already_terminal,
                then_bb: killed_bb,
                else_bb: stamp_bb,
            };

            let kc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(k)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(
                &mut step,
                stamp_bb,
                Inst::StoreField {
                    base: sv2,
                    field: ustr::Ustr::from(STATE),
                    value: kc,
                },
            );
            step.blocks[stamp_bb].term = if frame_slot_channel {
                Term::Return(Vec::new())
            } else {
                Term::Return(values)
            };
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }
    let num_states = resume_of_state.len();

    // 3. Normal returns -> stash `s.__return`, mark completed (-3), bare
    // return. The stamp is guarded on `state >= 0`: a body that killed its own
    // handle mid-run (state already -1) must STAY killed -- neither clobber
    // the terminal to -3 (completed() would lie) nor keep a stashed result a
    // kill promised not to have.
    for &bid in &orig_blocks {
        if yield_blocks.contains(&bid) {
            continue;
        }
        let ret_val = match (&step.blocks[bid].term, has_return_value) {
            // A co fn's completion value is single (never scalarized); take it.
            (Term::Return(vs), true) => Some(vs.first().copied()),
            (Term::Return(_), false) => Some(None),
            _ => None,
        };
        if let Some(v0) = ret_val {
            if let Some(v) = v0 {
                let sv = emit(&mut step, bid, Inst::Load(s_cell));
                emit(
                    &mut step,
                    bid,
                    Inst::StoreField {
                        base: sv,
                        field: ustr::Ustr::from("__return"),
                        value: v,
                    },
                );
            }
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let zero = emit(&mut step, bid, Inst::Const(Lit::Int(0)));
            let already_terminal = emit(&mut step, bid, Inst::IntLt(st, zero));
            let killed_bb = step.new_block();
            let stamp_bb = step.new_block();
            step.blocks[bid].term = Term::Branch {
                cond: already_terminal,
                then_bb: killed_bb,
                else_bb: stamp_bb,
            };

            let dc = emit(&mut step, stamp_bb, Inst::Const(Lit::Int(-3)));
            let sv2 = emit(&mut step, stamp_bb, Inst::Load(s_cell));
            emit(
                &mut step,
                stamp_bb,
                Inst::StoreField {
                    base: sv2,
                    field: ustr::Ustr::from(STATE),
                    value: dc,
                },
            );
            step.blocks[stamp_bb].term = Term::Return(Vec::new());

            if v0.is_some() {
                let nil = emit(&mut step, killed_bb, Inst::Const(Lit::Nil));
                let sv3 = emit(&mut step, killed_bb, Inst::Load(s_cell));
                emit(
                    &mut step,
                    killed_bb,
                    Inst::StoreField {
                        base: sv3,
                        field: ustr::Ustr::from("__return"),
                        value: nil,
                    },
                );
            }
            step.blocks[killed_bb].term = Term::Return(Vec::new());
        }
    }

    // 3b. Spill cross-block values (loop-carried collections/bounds, multi-yield
    // args) so they survive a suspend/resume — see `spill_cross_block_values`.
    let spill_values =
        spill_cross_block_values(&mut step, s_cell, &orig_blocks, &suspension_resumes);

    // 3c. Frame layout stamp. Hash everything that fixes the frame's field
    // meanings + the state numbering: (cell idx, source name) pairs, the spill
    // field set, per-state variant tags, and the yield-channel shape. Body
    // LOGIC/values/spans are deliberately excluded so a layout-stable edit keeps
    // live coroutines running (the live-update feature). The ctor writes the
    // stamp; the step's entry (below) rejects a frame with a different one --
    // through `error()`, so the pcall in `__advance` turns it into a clean
    // errored kill.
    let mut canon = String::new();
    {
        use std::fmt::Write as _;
        // Slot order == raw-id order (dense_slots numbers by raw id, cells.iter()
        // yields by raw id), so hashing `c<slot>:<name>` here matches both the
        // emitted field names and the slot ordinals.
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
                let _ = write!(canon, "ch:{}:{}", ch.enum_name, ch.tuple_width);
                for rf in &ch.record_fields {
                    let _ = write!(canon, ":{rf}");
                }
            }
            None => canon.push_str("ch:-"),
        }
    }
    let layout_hash = fnv1a32(&canon) as i64;

    // 4. Dispatch entry: read s.__state, branch to each resume block; fall
    // through to the original entry (state 0). A chain of one block per state.
    let orig_entry = step.entry;
    if num_states == 0 {
        let d = step.new_block();
        step.blocks[d].term = Term::Goto(orig_entry);
        step.entry = d;
    } else {
        // Build check blocks bottom-up so each `else` targets the next check.
        let check_blocks: Vec<Idx<Block>> = (0..num_states).map(|_| step.new_block()).collect();
        for k in 0..num_states {
            let bid = check_blocks[k];
            let sv = emit(&mut step, bid, Inst::Load(s_cell));
            let st = emit(
                &mut step,
                bid,
                Inst::Field {
                    base: sv,
                    field: ustr::Ustr::from(STATE),
                },
            );
            let kc = emit(&mut step, bid, Inst::Const(Lit::Int(k as i64 + 1)));
            let eq = emit(&mut step, bid, Inst::Eq(st, kc));
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

    // 4b. Layout-stamp check ABOVE the dispatch: a frame written by a
    // different layout (a live coroutine surviving a layout-shifting reload)
    // must not be driven -- raise instead; `__advance`'s pcall turns it into a
    // clean errored kill (on_killed fires, last_error explains, the poller
    // logs). A fresh coroutine always matches (ctor + step emitted together).
    {
        let dispatch = step.entry;
        let chk = step.new_block();
        let sv = emit(&mut step, chk, Inst::Load(s_cell));
        let lay = emit(
            &mut step,
            chk,
            Inst::Field {
                base: sv,
                field: ustr::Ustr::from("__layout"),
            },
        );
        let kc = emit(&mut step, chk, Inst::Const(Lit::Int(layout_hash)));
        let ne = emit(&mut step, chk, Inst::NotEq(lay, kc));
        let raise = step.new_block();
        let err_fn = emit(&mut step, raise, Inst::Global(ustr::Ustr::from("error")));
        let msg = emit(
            &mut step,
            raise,
            Inst::Const(Lit::Str(ustr::Ustr::from(
                format!(
                    "coroutine killed: frame layout changed by hot reload ({})",
                    func.name
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

    // 5. The step is an ordinary fn `(s, __resume_arg)`.
    step.name = ustr::Ustr::from(format!("{}__step", func.name).as_str());
    step.params = vec![s_cell, resume_arg];
    step.is_coroutine = false;
    step.resume_arg = None;

    CoroLowered {
        step,
        s_cell,
        frame_plan: CoroutineFramePlan::new(frame_cells, frame_slots, spill_values, layout_hash),
        params,
        num_states,
        hooks,
        step_tree: tree,
        step_own: own,
    }
}

/// Dense per-frame slot map: the given cells numbered 0.. by raw-id order.
fn dense_slots(cells: impl Iterator<Item = Idx<Cell>>) -> HashMap<Idx<Cell>, u32> {
    let mut v: Vec<Idx<Cell>> = cells.collect();
    v.sort_by_key(|c| raw(*c));
    v.into_iter()
        .enumerate()
        .map(|(i, c)| (c, i as u32))
        .collect()
}

impl CoroLowered {
    /// Frame cells owned by the coroutine's root and therefore shared with a
    /// detached hook/defer body. Cells introduced only by that detached body
    /// remain ordinary locals in its own `MirFunc`.
    pub(crate) fn root_frame_info(&self) -> FrameInfo {
        let fields: HashSet<Idx<Cell>> = self
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|c| {
                self.params.contains(c)
                    || self
                        .step_own
                        .owner
                        .get(c)
                        .is_some_and(|owner| *owner == crate::scope::ROOT)
            })
            .collect();
        let slots = self
            .frame_plan
            .slots()
            .iter()
            .filter(|(cell, _)| fields.contains(cell))
            .map(|(&cell, &slot)| (cell, slot))
            .collect();
        FrameInfo {
            s_cell: self.s_cell,
            fields,
            slots,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower;

    #[test]
    fn coroutine_transform_exposes_one_owned_frame_plan() {
        let func = lower(
            "co fn f(x: int) -> Coroutine<int> { let saved = x yield saved }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        assert!(
            !lowered.frame_plan.slots().is_empty(),
            "the coroutine fixture must have a nonzero frame population"
        );
        assert_eq!(
            lowered.frame_plan.slots().len(),
            lowered.frame_plan.fields().len(),
            "one frame plan must own both membership and physical slots"
        );
    }

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
    fn coroutine_frame_cells_retain_types_across_conditional_yields() {
        let func = lower(
            "co fn f(flag: bool) -> Coroutine<int> {\n\
                 if flag { yield 1 yield 2 } else { yield 10 yield 20 }\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_cells_retain_types_across_null_coalesce() {
        let func = lower(
            "co fn f(x: int?) -> Coroutine<int> {\n\
                 yield x ?? -1\n\
                 yield x ?? -2\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
            lowered.step
        );
    }

    #[test]
    fn coroutine_frame_cells_retain_types_across_tuple_destructure() {
        let func = lower(
            "fn pair() -> (int, int) { return (10, 20) }\n\
             co fn f() -> Coroutine<int> {\n\
                 let (a, b) = pair()\n\
                 yield a\n\
                 yield b\n\
             }",
            "f",
        );
        let lowered = transform_coroutine(&func, None, false);
        let untyped: Vec<_> = lowered
            .frame_plan
            .fields()
            .iter()
            .copied()
            .filter(|&cell| lowered.step.cells[cell].ty.is_none())
            .map(|cell| (cell, lowered.step.cells[cell].name))
            .collect();
        assert!(
            untyped.is_empty(),
            "untyped frame cells: {untyped:?}\n{}",
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
