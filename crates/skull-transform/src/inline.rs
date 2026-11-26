//! Annotation gating bounds how many chunks a body edit invalidates.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smallvec::SmallVec;
use std::cell::Cell as ThreadCell;
use std::sync::{Arc, Mutex};

use bone_core::prelude::{BoundTree, CompilerError};
use bone_core::workspace::Workspace;
use comfy_la_arena::{ArenaMap, Idx, IdxRange};
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BackendFnAbiPlanHandle, ScalarCtx};
use skull_mir::mir::*;
use ustr::Ustr;

use crate::{top_level_blocks, visit::raw};

const MAX_INLINES_PER_FN: usize = 1000;

thread_local! {
    static INLINE_OFF: ThreadCell<bool> = const { ThreadCell::new(false) };
}

pub struct InlineOffGuard {
    previous: bool,
}

pub fn inline_off_guard() -> InlineOffGuard {
    inline_mode_guard(true)
}

pub fn inline_mode_guard(off: bool) -> InlineOffGuard {
    let previous = INLINE_OFF.with(|cell| cell.replace(off));
    InlineOffGuard { previous }
}

pub fn inline_off_active() -> bool {
    skull_mir::pass_disabled("inline") || INLINE_OFF.with(|off| off.get())
}

impl Drop for InlineOffGuard {
    fn drop(&mut self) {
        INLINE_OFF.with(|off| off.set(self.previous));
    }
}

#[derive(Copy, Clone, PartialEq)]
enum Kind {
    Straight,
    General,
}

#[derive(Clone)]
struct CalleeBody {
    func: MirFunc,
    /// The concrete body this snapshot was lowered from. A declaration-level
    /// snapshot is not a valid source for a concrete splice.
    subject: Instance,
    abi: BackendFnAbiPlanHandle,
    kind: Kind,
    ret_arity: usize,
    reachable: Vec<Idx<Block>>,
    insts: usize,
    /// Coalescing preserves field mutation through a shared argument. A
    /// parameter assigned as a whole needs its own cell.
    coalescable_params: Vec<bool>,
    /// A lambda sub-CFG reads captured cells, so its body cannot forward these reads.
    forwardable_reads: Vec<(usize, Idx<Inst>)>,
    forwards_reads: bool,
    resolves_bounds: bool,
    /// Spliced for its size, not an annotation: a `@noalloc` caller keeps the
    /// call so the allocation check sees the callee it would reject.
    small: bool,
}

/// Rejected candidates are cached too.
type CacheKey = Instance;

#[derive(Default)]
struct Classified {
    body: Option<Arc<CalleeBody>>,
    /// Emission takes the raw body once. The splice template is independent,
    /// and emission can lower again if a later caller needs the raw body.
    lowered: Mutex<Option<Box<MirFunc>>>,
}

pub struct InlineCandidateTable {
    entries: HashMap<CacheKey, Classified>,
    removed: HashSet<CacheKey>,
    parent: Option<Arc<InlineCandidateTable>>,
}

pub struct InlineCandidateWarm {
    key: CacheKey,
    candidate: Classified,
}

impl InlineCandidateTable {
    pub fn from_warms(warms: Vec<InlineCandidateWarm>) -> Self {
        Self {
            entries: Self::warm_entries(warms),
            removed: HashSet::default(),
            parent: None,
        }
    }

    fn warm_entries(warms: Vec<InlineCandidateWarm>) -> HashMap<CacheKey, Classified> {
        let mut entries = HashMap::with_capacity_and_hasher(warms.len(), Default::default());
        for warm in warms {
            let previous = entries.insert(warm.key, warm.candidate);
            assert!(
                previous.is_none(),
                "inline candidate table received a duplicate key"
            );
        }
        entries
    }

    pub fn from_warms_incremental(
        base: Arc<Self>,
        warms: Vec<InlineCandidateWarm>,
        removed: &[Instance],
    ) -> Self {
        let entries = Self::warm_entries(warms);
        let mut removed_keys = removed.iter().cloned().collect::<HashSet<_>>();
        for key in entries.keys() {
            removed_keys.remove(key);
        }
        Self {
            entries,
            removed: removed_keys,
            parent: Some(base),
        }
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        self.lookup(key).is_some()
    }

    /// Compare the eligibility facts required to reuse a residual caller.
    pub fn residual_eligibility_matches(&self, other: &Self, key: &Instance) -> bool {
        let (Some(before), Some(after)) = (
            self.lookup(key).and_then(|c| c.body.as_deref()),
            other.lookup(key).and_then(|c| c.body.as_deref()),
        ) else {
            return false;
        };

        before.subject == after.subject
            && before.abi == after.abi
            && before.func.params.len() == after.func.params.len()
            && before.ret_arity == after.ret_arity
            && before.small == after.small
    }

    pub fn keys(&self) -> Vec<Instance> {
        let mut keys = self
            .parent
            .as_ref()
            .map(|parent| parent.keys())
            .unwrap_or_default();
        keys.retain(|key| !self.removed.contains(key));
        keys.extend(self.entries.keys().cloned());
        keys.sort();
        keys.dedup();
        keys
    }

    fn take_lowered_body(&self, key: &Instance) -> Option<MirFunc> {
        let body = self
            .lookup(key)?
            .lowered
            .lock()
            .expect("lowered inline candidate")
            .take();
        body.map(|body| *body)
    }

    fn lookup(&self, key: &CacheKey) -> Option<&Classified> {
        if let Some(candidate) = self.entries.get(key) {
            return Some(candidate);
        }
        if self.removed.contains(key) {
            return None;
        }
        self.parent.as_ref().and_then(|parent| parent.lookup(key))
    }
}

#[derive(Clone, Default)]
pub struct InlineCache {
    cands: HashMap<CacheKey, Option<Arc<CalleeBody>>>,
    callback_cands: HashMap<CacheKey, Option<Arc<CalleeBody>>>,
    shared: Option<Arc<InlineCandidateTable>>,
    scratch: InlineScratch,
}

#[derive(Clone, Default)]
struct InlineScratch {
    bound_inst_map: HashMap<Idx<Inst>, Idx<Inst>>,
    cell_map: ArenaMap<Idx<Cell>, Idx<Cell>>,
    block_map: ArenaMap<Idx<Block>, Idx<Block>>,
    overrides: HashMap<Idx<Cell>, Idx<Cell>>,
    coalesced: Vec<bool>,
    inst_ids: IdxRange<Inst>,
    added_blocks: Vec<Idx<Block>>,
    replacement: Vec<Idx<Inst>>,
    predecessors: Option<Predecessors>,
}

type Predecessors = Vec<SmallVec<[Idx<Block>; 2]>>;

// Resolve operands as sites are visited, then fix up surviving users once.
#[derive(Default)]
struct ResultAliases {
    values: ArenaMap<Idx<Inst>, Idx<Inst>>,
}

impl ResultAliases {
    fn resolve(&self, mut value: Idx<Inst>) -> Idx<Inst> {
        while let Some(&other) = self.values.get(value) {
            value = other;
        }
        value
    }

    fn finish(self, func: &mut MirFunc) {
        if self.values.values().next().is_none() {
            return;
        }
        let remap = |value| self.resolve(value);
        for (_, block) in func.blocks.iter_mut() {
            for &id in &block.insts {
                func.insts[id].remap_uses(&remap);
            }
            block.term.remap_uses(&remap);
        }
    }
}

impl InlineCache {
    pub fn with_candidate_table(table: Arc<InlineCandidateTable>) -> Self {
        Self {
            shared: Some(table),
            ..Self::default()
        }
    }

    pub fn candidate_table(&self) -> Option<Arc<InlineCandidateTable>> {
        self.shared.clone()
    }

    pub fn take_lowered_body(&self, key: &Instance) -> Option<MirFunc> {
        self.shared.as_ref()?.take_lowered_body(key)
    }

    pub fn replace_candidate_table(&mut self, table: Arc<InlineCandidateTable>) {
        self.cands.clear();
        self.callback_cands.clear();
        self.shared = Some(table);
    }

    pub fn warm_candidate(
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: Instance,
        concrete_plans: &ConcretePlans,
    ) -> Result<InlineCandidateWarm, CompilerError> {
        let candidate = classify(ws, scx, &inst, concrete_plans, true, false)?;
        Ok(InlineCandidateWarm {
            key: inst,
            candidate,
        })
    }

    pub fn invalidate(&mut self, fref: FuncRef) {
        self.cands.retain(|inst, _| inst.func != fref);
        self.callback_cands.retain(|inst, _| inst.func != fref);
    }
}

fn explicit_inline(f: &bone_core::hir::Function) -> bool {
    f.annotations
        .iter()
        .any(|a| matches!(a.name.name.as_str(), "inline" | "pure"))
}

#[derive(Default)]
struct LiveClosure {
    blocks: HashSet<Idx<Block>>,
    insts: HashSet<Idx<Inst>>,
    cells: HashSet<Idx<Cell>>,
}

/// Share these tables when compacting several regions from one body.
pub(crate) struct ArenaFacts {
    lambda_params: Vec<(u32, u32)>,
    /// `(emit cell, emit block, block)` of every cell an instruction names,
    /// sorted and unique by the emit pair.
    uses: Vec<(u32, u32, Idx<Block>)>,
    loads: Vec<(Idx<Block>, Idx<Cell>)>,
}

impl ArenaFacts {
    pub(crate) fn of(func: &MirFunc) -> Self {
        let mut lambda_params = Vec::new();
        for (_, inst) in func.insts.iter() {
            if let Inst::Lambda { params, entry } = inst {
                for &cell in params {
                    lambda_params.push((func.emit_cell_id(cell), func.emit_block_id(*entry)));
                }
            }
        }
        lambda_params.sort_unstable();
        lambda_params.dedup();
        let mut uses = Vec::new();
        let mut loads = Vec::new();
        for (bid, block) in func.blocks.iter() {
            let emit_block = func.emit_block_id(bid);
            for &iid in &block.insts {
                let inst = &func.insts[iid];
                inst.cells_each(&mut |cell| uses.push((func.emit_cell_id(cell), emit_block, bid)));
                if let Inst::Load(cell) = inst {
                    loads.push((bid, *cell));
                }
            }
        }
        uses.sort_unstable_by_key(|&(cell, block, _)| (cell, block));
        uses.dedup_by_key(|&mut (cell, block, _)| (cell, block));
        Self {
            lambda_params,
            uses,
            loads,
        }
    }

    /// Cells named outside the top-level blocks reached from `entry`, plus
    /// every lambda parameter, as sorted `(emit cell, emit block)` pairs.
    fn forced_cells(&self, func: &MirFunc, entry: Idx<Block>) -> Vec<(u32, u32)> {
        let mut top = vec![false; func.blocks.len()];
        for block in crate::visit::top_level_blocks_from(func, entry) {
            top[crate::visit::raw(block)] = true;
        }
        let mut cells: Vec<(u32, u32)> = self
            .uses
            .iter()
            .filter(|(_, _, block)| !top[crate::visit::raw(*block)])
            .map(|&(cell, block, _)| (cell, block))
            .collect();
        if !self.lambda_params.is_empty() {
            cells.extend(self.lambda_params.iter().copied());
            cells.sort_unstable();
            cells.dedup();
        }
        cells
    }
}

fn live_closure(func: &MirFunc) -> LiveClosure {
    live_closure_at(func, func)
}

fn live_closure_at(header: &MirFunc, func: &MirFunc) -> LiveClosure {
    let mut live = LiveClosure::default();
    let mut blocks = vec![header.entry];
    let mut insts = Vec::new();
    live.cells.extend(header.params.iter().copied());
    loop {
        while let Some(bid) = blocks.pop() {
            if !live.blocks.insert(bid) {
                continue;
            }
            let block = &func.blocks[bid];
            insts.extend(block.insts.iter().copied());
            block.term.uses_each(&mut |value| insts.push(value));
            skull_mir::cfg::successors(&block.term, |target| blocks.push(target));
        }
        let Some(iid) = insts.pop() else {
            break;
        };
        if !live.insts.insert(iid) {
            continue;
        }
        let inst = &func.insts[iid];
        inst.uses_each(&mut |operand| insts.push(operand));
        inst.cells_each(&mut |cell| {
            live.cells.insert(cell);
        });
        if let Inst::Lambda { entry, .. } = inst {
            blocks.push(*entry);
        }
    }
    if let Some(cell) = header.resume_arg {
        live.cells.insert(cell);
    }
    for (flag, block) in &header.coro_defers {
        if live.blocks.contains(block) {
            live.cells.insert(*flag);
        }
    }
    for iid in &live.insts {
        if let Some(plan) = func.closure_env_plans.get(iid) {
            live.cells.extend(plan.captures().iter().copied());
            if let Some(cell) = plan.env_cell() {
                live.cells.insert(cell);
            }
        }
    }
    live
}

macro_rules! copy_inst_tables {
    ($target:expr, $source:expr, $mapped:expr, $($field:ident),+ $(,)?) => {
        $(
            for (old_id, value) in &$source.$field {
                if let Some(new_id) = $mapped(*old_id) {
                    $target.$field.insert(new_id, value.clone());
                }
            }
        )+
    };
}

fn copy_inst_metadata(
    target: &mut MirFunc,
    source: &MirFunc,
    mapped: impl Fn(Idx<Inst>) -> Option<Idx<Inst>> + Copy,
) {
    copy_inst_tables!(
        target,
        source,
        mapped,
        spec_bindings,
        callee_instances,
        call_abi_plans,
        call_rep_plans,
        flat_call_lanes,
        fn_value_symbols,
        code_value_identities,
        lambda_abis,
    );
    target.binding_initializers.extend(
        source
            .binding_initializers
            .iter()
            .filter_map(|id| mapped(*id)),
    );
    for (old_id, span) in source.inst_spans.iter() {
        if let Some(new_id) = mapped(old_id) {
            target.inst_spans.insert(new_id, *span);
        }
    }
}

fn copy_projection_metadata(
    target: &mut MirFunc,
    source: &MirFunc,
    mapped: impl Fn(Idx<Inst>) -> Option<Idx<Inst>> + Copy,
) {
    target.synthetic_projections.extend(
        source
            .synthetic_projections
            .iter()
            .filter_map(|id| mapped(*id)),
    );
    copy_inst_tables!(target, source, mapped, flat_box_widths);
}

fn compact_live_template(
    mut source: MirFunc,
    live: &LiveClosure,
) -> (MirFunc, ArenaMap<Idx<Block>, Idx<Block>>) {
    let nested = std::mem::take(&mut source.nested_fns);
    let facts = ArenaFacts::of(&source);
    compact_region(&source, &source, &facts, nested, live, &[], false)
}

fn compact_region(
    header: &MirFunc,
    arena: &MirFunc,
    facts: &ArenaFacts,
    nested: Vec<MirFunc>,
    live: &LiveClosure,
    extra_cleanup_loads: &[Idx<Cell>],
    split_for_emission: bool,
) -> (MirFunc, ArenaMap<Idx<Block>, Idx<Block>>) {
    let source_inst_span = arena.emit_inst_span();
    let source_cell_span = arena.emit_cell_span();
    let source_block_span = arena.emit_block_span();
    let source_forced_cells = facts.forced_cells(arena, header.entry);
    let mut live_cells: Vec<Idx<Cell>> = live.cells.iter().copied().collect();
    live_cells.sort_unstable_by_key(|id| u32::from(id.into_raw()));
    let mut live_blocks: Vec<Idx<Block>> = live.blocks.iter().copied().collect();
    live_blocks.sort_unstable_by_key(|id| u32::from(id.into_raw()));
    let mut live_insts: Vec<Idx<Inst>> = live.insts.iter().copied().collect();
    live_insts.sort_unstable_by_key(|id| u32::from(id.into_raw()));

    let mut cell_map = ArenaMap::default();
    let mut cells = comfy_la_arena::Arena::with_capacity(live.cells.len());
    for &id in &live_cells {
        cell_map.insert(id, cells.alloc(arena.cells[id].clone()));
    }

    let mut block_map = ArenaMap::default();
    let mut blocks = comfy_la_arena::Arena::with_capacity(live.blocks.len());
    for &id in &live_blocks {
        block_map.insert(id, blocks.alloc(Block::new()));
    }

    let mut inst_map = HashMap::default();
    let mut insts = comfy_la_arena::Arena::with_capacity(live.insts.len());
    for &id in &live_insts {
        inst_map.insert(id, insts.alloc(arena.insts[id].clone()));
    }
    let mapped = |id| inst_map.get(&id).copied();
    let remap = |id| inst_map[&id];
    for (_, inst) in insts.iter_mut() {
        remap_clone(inst, &remap, &cell_map, &block_map);
    }
    for (old_id, &new_id) in block_map.iter() {
        blocks[new_id] = remap_block(&arena.blocks[old_id], &remap, &block_map);
    }

    let mut template = MirFunc::new(header.name);
    template.code_identity = header.code_identity.clone();
    template.code_abi_fingerprint = header.code_abi_fingerprint.clone();
    template.code_compatibility = header.code_compatibility.clone();
    template.nested_fn_identities = arena.nested_fn_identities.clone();
    template.rep_arena = arena.rep_arena.clone();
    template.params = header.params.iter().map(|param| cell_map[*param]).collect();
    template.abi_plan = header.abi_plan.clone();
    template.abi_representation = header.abi_representation.clone();
    template.statics = header.statics.clone();
    template.comptime_reads = arena.comptime_reads.clone();
    template.struct_registry_roots = arena.struct_registry_roots.clone();
    template.insts = insts;
    template.cells = cells;
    template.blocks = blocks;
    // Kept cells loaded only outside the region, so cleanup still counts them.
    let mut cleanup_load_cells: Vec<Idx<Cell>> = facts
        .loads
        .iter()
        .filter_map(|(block, cell)| {
            let mapped = cell_map.get(*cell).copied()?;
            (!live.blocks.contains(block)).then_some(mapped)
        })
        .collect();
    cleanup_load_cells.extend(
        extra_cleanup_loads
            .iter()
            .filter_map(|cell| cell_map.get(*cell).copied()),
    );
    cleanup_load_cells.sort_unstable_by_key(|cell| u32::from(cell.into_raw()));
    cleanup_load_cells.dedup();
    template.inline_cleanup_load_cells = cleanup_load_cells;
    let (forced_cells, forced_cell_blocks) = source_forced_cells.into_iter().unzip();
    template.emit_ids = Some(EmitIds {
        insts: live_insts
            .into_iter()
            .map(|id| arena.emit_inst_id(id))
            .collect(),
        cells: live_cells
            .into_iter()
            .map(|id| arena.emit_cell_id(id))
            .collect(),
        blocks: live_blocks
            .into_iter()
            .map(|id| arena.emit_block_id(id))
            .collect(),
        forced_cells,
        forced_cell_blocks,
        next_insts: source_inst_span,
        next_cells: source_cell_span,
        next_blocks: source_block_span,
    });
    template.entry = block_map[header.entry];
    template.is_coroutine = header.is_coroutine;
    template.resume_arg = header.resume_arg.map(|cell| cell_map[cell]);
    template.coro_defers = header
        .coro_defers
        .iter()
        .filter(|&(_, block)| live.blocks.contains(block))
        .map(|(flag, block)| (cell_map[*flag], block_map[*block]))
        .collect();
    template.for_iter_headers = arena
        .for_iter_headers
        .iter()
        .filter(|header| live.blocks.contains(header))
        .map(|header| block_map[*header])
        .collect();
    template.hoisted_fn_globals = arena.hoisted_fn_globals.clone();

    copy_inst_metadata(&mut template, arena, mapped);
    copy_projection_metadata(&mut template, arena, mapped);
    copy_inst_tables!(template, arena, mapped, bound_trees, resolved_bound_trees);
    if split_for_emission {
        template.materialized_default_guards.extend(
            arena
                .materialized_default_guards
                .iter()
                .filter_map(|id| inst_map.get(id)),
        );
    }
    for (old_id, plan) in &arena.closure_env_plans {
        let Some(&new_id) = inst_map.get(old_id) else {
            continue;
        };
        let captures = plan.captures().iter().map(|cell| cell_map[*cell]).collect();
        let env_cell = plan.env_cell().map(|cell| cell_map[cell]);
        template.closure_env_plans.insert(
            new_id,
            skull_mir::mir::ClosureEnvPlan::new(captures, env_cell),
        );
    }
    template.bound_next_split.extend(
        arena
            .bound_next_split
            .iter()
            .filter_map(|id| inst_map.get(id)),
    );
    import_call_sites(
        &mut template,
        arena,
        mapped,
        Some("call-site owner must exist in the candidate template"),
    );

    template.nested_fns = nested
        .into_iter()
        .map(|nested| {
            let nested_live = live_closure(&nested);
            compact_live_template(nested, &nested_live).0
        })
        .collect();
    (template, block_map)
}

pub(crate) fn compact_split_body(header: &MirFunc, arena: &MirFunc, facts: &ArenaFacts) -> MirFunc {
    let live = live_closure_at(header, arena);
    compact_region(
        header,
        arena,
        facts,
        Vec::new(),
        &live,
        &arena.inline_cleanup_load_cells,
        true,
    )
    .0
}

fn map_forced_cells(
    caller: &MirFunc,
    callee: &MirFunc,
    overrides: &HashMap<Idx<Cell>, Idx<Cell>>,
    cell_base: u32,
    block_base: u32,
) -> Vec<(u32, u32)> {
    let source = callee.emit_ids.as_ref().expect("compacted inline template");
    source
        .forced_cells
        .iter()
        .zip(source.forced_cell_blocks.iter().copied())
        .map(|(&source_id, source_block)| {
            let target_cell = if let Some(target) = callee.cells.iter().find_map(|(cell, _)| {
                (callee.emit_cell_id(cell) == source_id)
                    .then(|| overrides.get(&cell).copied())
                    .flatten()
            }) {
                caller.emit_cell_id(target)
            } else {
                let skipped = overrides
                    .keys()
                    .filter(|cell| callee.emit_cell_id(**cell) < source_id)
                    .count() as u32;
                cell_base + source_id - skipped
            };
            (target_cell, block_base + source_block)
        })
        .collect()
}

fn classify(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
    retain_lowered: bool,
    callback: bool,
) -> Result<Classified, CompilerError> {
    let fref = inst.func;
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern || f.inline_body.is_some() || f.is_coroutine {
        return Ok(Classified::default());
    }
    let has_bounds = fn_has_bounds(f);
    let small = !callback && !has_bounds && !explicit_inline(f);
    if small && (!f.annotations.is_empty() || !small_source(ws, fref)) {
        return Ok(Classified::default());
    }
    let Some(plan) = concrete_plans.get(inst) else {
        return Ok(Classified::default());
    };
    let lowered = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, concrete_plans);
    // The callee's own emission reports why an unannotated body fails to lower.
    let mf = match lowered {
        Err(_) if small || callback => return Ok(Classified::default()),
        lowered => lowered?,
    };
    let abi = Arc::clone(mf.abi_plan.as_ref().expect("lowered instance ABI"));
    let (template, lowered) = if retain_lowered {
        (mf.clone(), Some(Box::new(mf)))
    } else {
        (mf, None)
    };
    let max_insts = if callback {
        MAX_CALLBACK_CALLEE_INSTS
    } else if small {
        SMALL_CALLEE_INSTS
    } else {
        usize::MAX
    };
    let body = shape(inst, has_bounds, small, callback, max_insts, abi, template)?;
    Ok(Classified {
        body: body.map(Arc::new),
        lowered: Mutex::new(lowered),
    })
}

fn shape(
    inst: &Instance,
    has_bounds: bool,
    small: bool,
    callback: bool,
    max_insts: usize,
    abi: BackendFnAbiPlanHandle,
    mut mf: MirFunc,
) -> Result<Option<CalleeBody>, CompilerError> {
    let fref = inst.func;
    crate::cleanup(&mut mf)?;
    let mut has_lambda = false;
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Lambda { .. } => {
                // A bounded body can inline a nested lambda only through the CFG splice.
                if !has_bounds {
                    return Ok(None);
                }
                has_lambda = true;
            }
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } if *func == fref => {
                return Ok(None);
            }
            _ => {}
        }
    }

    let source_reachable = top_level_blocks(&mf);
    // An ordinary caller cannot supply the callee's coroutine state.
    for &b in &source_reachable {
        match &mf.blocks[b].term {
            Term::Yield { .. } => return Ok(None),
            _ => {}
        }
    }
    let reachable_insts: usize = source_reachable
        .iter()
        .map(|&b| mf.blocks[b].insts.len())
        .sum();
    if reachable_insts > max_insts || ((small || callback) && has_back_edge(&mf, &source_reachable))
    {
        return Ok(None);
    }

    let mut rebound: HashSet<Idx<Cell>> = HashSet::default();
    for (_, inst) in mf.insts.iter() {
        inst.cell_writes_each(&mut |cell| {
            rebound.insert(cell);
        });
    }
    let coalescable_params: Vec<bool> = mf.params.iter().map(|p| !rebound.contains(p)).collect();
    let live = live_closure(&mf);
    let (mf, block_map) = compact_live_template(mf, &live);

    let mut reachable = Vec::new();
    let (kind, ret_arity) = match &mf.blocks[mf.entry].term {
        Term::Return(values) if !has_lambda => (Kind::Straight, values.len()),
        _ => {
            reachable.extend(source_reachable.iter().map(|block| block_map[*block]));
            let mut arities = reachable.iter().filter_map(|&b| match &mf.blocks[b].term {
                Term::Return(vs) => Some(vs.len()),
                _ => None,
            });
            let Some(ret_arity) = arities.next() else {
                return Ok(None);
            };
            if !arities.all(|arity| arity == ret_arity) {
                return Ok(None);
            }
            (Kind::General, ret_arity)
        }
    };
    let forwardable_reads = if has_lambda {
        Vec::new()
    } else {
        let blocks = match kind {
            Kind::Straight => std::slice::from_ref(&mf.entry),
            Kind::General => &reachable,
        };
        forwardable_param_reads(&mf, &coalescable_params, blocks)
    };
    let resolves_bounds = !mf.bound_trees.is_empty()
        || mf
            .insts
            .iter()
            .any(|(_, inst)| matches!(inst, Inst::CallBoundMulti { .. }));
    Ok(Some(CalleeBody {
        func: mf,
        subject: inst.clone(),
        abi,
        kind,
        ret_arity,
        reachable,
        insts: reachable_insts,
        coalescable_params,
        forwardable_reads,
        forwards_reads: !has_lambda,
        resolves_bounds,
        small,
    }))
}

/// Unannotated callees at most this many live instructions, with no loop, are
/// spliced like `@inline` ones. A call costs LuaJIT a trace budget either way:
/// a tail call counts against `loopunroll` for the rest of the trace, and any
/// other call keeps the caller's frame under the 250-slot trace limit.
const SMALL_CALLEE_INSTS: usize = 16;

/// Bodies above this many HIR expressions and statements are never lowered
/// to measure them; none of them fits `SMALL_CALLEE_INSTS`.
const SMALL_CALLEE_HIR_NODES: usize = 50;

fn small_source(ws: &Workspace, fref: FuncRef) -> bool {
    ws.module(fref.module)
        .bodies
        .get(&bone_core::hir::BodyOwner::Function(fref.func))
        .is_some_and(|body| {
            body.expr_arena.len() + body.statement_arena.len() <= SMALL_CALLEE_HIR_NODES
        })
}

fn has_back_edge(mf: &MirFunc, blocks: &[Idx<Block>]) -> bool {
    if blocks.len() == 1 {
        return matches!(
            mf.blocks[mf.entry].term,
            Term::Goto(_) | Term::Branch { .. }
        );
    }
    let mut order = vec![usize::MAX; mf.blocks.len()];
    for (i, b) in skull_mir::cfg::reverse_postorder(mf, mf.entry)
        .into_iter()
        .enumerate()
    {
        order[raw(b)] = i;
    }
    blocks.iter().any(|&b| {
        let back = |target: Idx<Block>| order[raw(target)] <= order[raw(b)];
        match &mf.blocks[b].term {
            Term::Goto(target) => back(*target),
            Term::Branch {
                then_bb, else_bb, ..
            } => back(*then_bb) || back(*else_bb),
            _ => false,
        }
    })
}

fn forward_param_reads(
    func: &mut MirFunc,
    cand: &CalleeBody,
    args: &[Idx<Inst>],
    scratch: &InlineScratch,
) -> SmallVec<[(Idx<Inst>, Idx<Inst>); 8]> {
    let mut forwarded: SmallVec<[(Idx<Inst>, Idx<Inst>); 8]> = SmallVec::new();
    for &(p, read) in &cand.forwardable_reads {
        if !scratch.coalesced[p] && forwards_arg(func, args[p]) {
            forwarded.push((splice_inst(scratch.inst_ids, read), args[p]));
        }
    }
    if !forwarded.is_empty() {
        let to = |v: Idx<Inst>| forwarded_to(&forwarded, v);
        for iid in scratch.inst_ids {
            func.insts[iid].remap_uses(&to);
        }
        for source_block in &cand.reachable {
            func.blocks[scratch.block_map[*source_block]]
                .term
                .remap_uses(&to);
        }
    }
    forwarded
}

fn forwarded_to(forwarded: &[(Idx<Inst>, Idx<Inst>)], v: Idx<Inst>) -> Idx<Inst> {
    forwarded
        .iter()
        .find(|(read, _)| *read == v)
        .map_or(v, |(_, arg)| *arg)
}

/// Whether every read of parameter `i` is rewired to its argument, so a copy
/// into the parameter cell would have no reader.
fn copy_unread(cand: &CalleeBody, i: usize) -> bool {
    cand.forwards_reads && cand.coalescable_params.get(i).copied().unwrap_or(false)
}

fn forwards_arg(func: &MirFunc, arg: Idx<Inst>) -> bool {
    !matches!(
        func.insts[arg],
        Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeTuple(..)
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

fn forwardable_param_reads(
    mf: &MirFunc,
    coalescable: &[bool],
    blocks: &[Idx<Block>],
) -> Vec<(usize, Idx<Inst>)> {
    let mut reads = Vec::new();
    for i in blocks
        .iter()
        .flat_map(|&b| mf.blocks[b].insts.iter().copied())
    {
        let Inst::Load(cell) = mf.insts[i] else {
            continue;
        };
        let Some(p) = mf.params.iter().position(|&param| param == cell) else {
            continue;
        };
        if coalescable[p] {
            reads.push((p, i));
        }
    }
    reads
}

fn get<'a>(
    cache: &'a mut InlineCache,
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
) -> Result<Option<&'a CalleeBody>, CompilerError> {
    if !cache.cands.contains_key(inst) {
        if let Some(candidate) = cache.shared.as_ref().and_then(|table| table.lookup(inst)) {
            cache.cands.insert(inst.clone(), candidate.body.clone());
            return Ok(cache.cands[inst].as_deref());
        }
        let candidate = classify(ws, scx, inst, concrete_plans, false, false)?.body;
        cache.cands.insert(inst.clone(), candidate);
    }
    Ok(cache.cands[inst].as_deref())
}

/// The callees whose splices produced a call. Splicing a callee again inside
/// its own expansion would unroll recursion.
type SpliceChain = SmallVec<[FuncRef; 4]>;

struct Site {
    pos: (Idx<Block>, usize),
    call_id: Idx<Inst>,
    args: SmallVec<[Idx<Inst>; 4]>,
    dests: Option<SmallVec<[Idx<Cell>; 4]>>,
    inst: Instance,
    callbacks: SmallVec<[Idx<Inst>; 2]>,
}

type CallbackParam = (Instance, usize);

// Large wrapper splices increase the caller's LuaJIT trace stack.
const MAX_CALLBACK_CALLEE_INSTS: usize = 128;

impl CalleeBody {
    fn callback_forwards(&self, param: usize) -> Option<SmallVec<[CallbackParam; 2]>> {
        let mf = &self.func;
        if self.insts > MAX_CALLBACK_CALLEE_INSTS
            || !self.forwards_reads
            || !*self.coalescable_params.get(param)?
            || mf.cells[mf.params[param]].borrowed
        {
            return None;
        }
        let reads: HashSet<_> = self
            .forwardable_reads
            .iter()
            .filter_map(|&(p, read)| (p == param).then_some(read))
            .collect();
        let mut forwards = SmallVec::new();
        for (id, inst) in mf.insts.iter() {
            match inst {
                Inst::Call { args, .. } | Inst::CallMulti { args, .. } => {
                    for (pos, arg) in args.iter().enumerate() {
                        if reads.contains(arg) {
                            forwards.push((mf.callee_instances.get(&id)?.clone(), pos));
                        }
                    }
                }
                Inst::CallValue { args, .. } | Inst::CallValueMulti { args, .. } => {
                    if args.iter().any(|arg| reads.contains(arg)) {
                        return None;
                    }
                }
                Inst::Eq(a, b) | Inst::NotEq(a, b)
                    if matches!(mf.insts[*a], Inst::Const(Lit::Nil))
                        || matches!(mf.insts[*b], Inst::Const(Lit::Nil)) => {}
                _ => {
                    let mut escapes = false;
                    inst.uses_each(&mut |v| escapes |= reads.contains(&v));
                    if escapes {
                        return None;
                    }
                }
            }
        }
        for (_, block) in mf.blocks.iter() {
            let mut escapes = false;
            block
                .term
                .uses_each(&mut |value| escapes |= reads.contains(&value));
            if escapes {
                return None;
            }
        }
        Some(forwards)
    }
}

#[derive(Default)]
struct CallbackProof {
    summary: HashMap<CallbackParam, bool>,
    capturing: HashMap<Idx<Inst>, bool>,
    dependencies: HashSet<FuncRef>,
}

impl CallbackProof {
    fn captures(&mut self, mf: &MirFunc, lambda: Idx<Inst>) -> bool {
        if !self.capturing.contains_key(&lambda) {
            let tree = crate::scope::ScopeTree::build(mf);
            let own = tree.analyze(mf);
            self.capturing.extend(
                tree.scopes
                    .iter()
                    .zip(&own.captures)
                    .filter_map(|(scope, cells)| Some((scope.lambda?, !cells.is_empty()))),
            );
        }
        self.capturing.get(&lambda).copied().unwrap_or(false)
    }

    fn proves(
        &mut self,
        cache: &mut InlineCache,
        ws: &Workspace,
        scx: &ScalarCtx,
        plans: &ConcretePlans,
        key: CallbackParam,
    ) -> Result<bool, CompilerError> {
        if let Some(&value) = self.summary.get(&key) {
            return Ok(value);
        }
        let (inst, param) = &key;
        self.dependencies.insert(inst.func);
        // Inlining cannot discharge a recursive forwarding edge.
        self.summary.insert(key.clone(), false);
        let forwards = if let Some(cand) = get(cache, ws, scx, inst, plans)? {
            cand.callback_forwards(*param)
        } else {
            if !cache.callback_cands.contains_key(inst) {
                let candidate = classify(ws, scx, inst, plans, false, true)?.body;
                cache.callback_cands.insert(inst.clone(), candidate);
            }
            cache.callback_cands[inst]
                .as_ref()
                .and_then(|cand| cand.callback_forwards(*param))
        };
        let Some(forwards) = forwards else {
            return Ok(false);
        };
        for target in forwards {
            if !self.proves(cache, ws, scx, plans, target)? {
                return Ok(false);
            }
        }
        self.summary.insert(key, true);
        Ok(true)
    }
}

/// The delivery record must agree with the concrete owner already on the call.
fn call_target<'a>(
    func: &'a MirFunc,
    call_id: Idx<Inst>,
    fref: FuncRef,
    ws: &Workspace,
) -> Result<Option<(&'a Instance, &'a BackendFnAbiPlanHandle)>, CompilerError> {
    let Some(owner) = func.callee_instances.get(&call_id) else {
        return Ok(None);
    };
    if owner.func != fref
        && skull_mir::lower::canonical_variant(ws, owner.func)
            != skull_mir::lower::canonical_variant(ws, fref)
    {
        return Err(CompilerError::new_no_location(format!(
            "internal: call in `{}` records owner `{}` but targets another declaration",
            func.name,
            ws.module(owner.func.module).functions[owner.func.func].name,
        )));
    }
    let declaration = &ws.module(fref.module).functions[fref.func];
    if declaration.generics.is_empty() {
        if !owner.args.is_empty() {
            return Err(CompilerError::new_no_location(format!(
                "internal: non-generic call target `{}` carries type arguments",
                declaration.name,
            )));
        }
    } else if owner.args.is_empty() {
        return Ok(None);
    }
    if let Some(delivery) = func.spec_bindings.get(&call_id) {
        if delivery != owner {
            return Err(CompilerError::new_no_location(format!(
                "internal: delivery instance for `{}` disagrees with its concrete call owner",
                declaration.name,
            )));
        }
    }
    let abi = func.call_abi_plans.get(&call_id).ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "internal: concrete call to `{}` has no lowered ABI plan",
            declaration.name,
        ))
    })?;
    Ok(Some((owner, abi)))
}

/// Resume at the last splice: it only inserts calls at or after that position.
/// Scan the root CFG, since a closure body's cells are still upvalues.
fn find_site(
    func: &MirFunc,
    ws: &Workspace,
    scx: &ScalarCtx,
    cache: &mut InlineCache,
    self_ref: FuncRef,
    noalloc_caller: bool,
    top: &[bool],
    resume: Option<(Idx<Block>, usize)>,
    splice_chains: &HashMap<Idx<Inst>, SpliceChain>,
    concrete_plans: &ConcretePlans,
    callbacks: &mut CallbackProof,
    aliases: &ResultAliases,
) -> Result<Option<Site>, CompilerError> {
    let specialize_callbacks = !skull_mir::pass_disabled("lambda");
    let (first_block, first_inst) = resume.map_or((0, 0), |(block, index)| (raw(block), index));
    for (bid, block) in func.blocks.iter().skip(first_block) {
        if !top[raw(bid)] {
            continue;
        }
        let start = if raw(bid) == first_block {
            first_inst
        } else {
            0
        };
        for (idx, &iid) in block.insts.iter().enumerate().skip(start) {
            let (fref, args, dests) = match &func.insts[iid] {
                Inst::Call { func, args } => (*func, args, None),
                Inst::CallMulti { func, args, dests } => (*func, args, Some(dests)),
                _ => continue,
            };
            if fref == self_ref || splice_chains.get(&iid).is_some_and(|c| c.contains(&fref)) {
                continue;
            }
            let Some((inst, abi)) = call_target(func, iid, fref, ws)? else {
                continue;
            };
            let allows_normal = !noalloc_caller || {
                let owner = &ws.module(inst.func.module).functions[inst.func.func];
                explicit_inline(owner) || fn_has_bounds(owner)
            };
            let mut proven = SmallVec::new();
            if specialize_callbacks {
                for (param, &arg) in args.iter().enumerate() {
                    let arg = aliases.resolve(arg);
                    if matches!(func.insts[arg], Inst::Lambda { .. })
                        && (noalloc_caller || callbacks.captures(func, arg))
                        && callbacks.proves(
                            cache,
                            ws,
                            scx,
                            concrete_plans,
                            (inst.clone(), param),
                        )?
                    {
                        proven.push(arg);
                    }
                }
            }
            let candidate = if !proven.is_empty() {
                cache
                    .cands
                    .get(inst)
                    .and_then(Option::as_deref)
                    .or_else(|| cache.callback_cands.get(inst).and_then(Option::as_deref))
            } else if allows_normal {
                get(cache, ws, scx, inst, concrete_plans)?
            } else {
                None
            };
            let Some(cand) = candidate else {
                continue;
            };
            if &cand.subject != inst
                || cand.abi != *abi
                || cand.func.params.len() != args.len()
                || dests.map_or(cand.ret_arity > 1, |dests| cand.ret_arity != dests.len())
            {
                continue;
            }
            return Ok(Some(Site {
                pos: (bid, idx),
                call_id: iid,
                args: args.iter().map(|&arg| aliases.resolve(arg)).collect(),
                dests: dests.map(|dests| SmallVec::from_slice(dests)),
                inst: inst.clone(),
                callbacks: proven,
            }));
        }
    }
    Ok(None)
}

fn splice_inst(mut insts: IdxRange<Inst>, id: Idx<Inst>) -> Idx<Inst> {
    insts.nth(raw(id)).expect("inline instruction")
}

// Cells and blocks are mapped first. Instruction IDs form an appended range.
fn clone_body(caller: &mut MirFunc, callee: &MirFunc, scratch: &mut InlineScratch, kind: Kind) {
    caller.ensure_emit_ids();
    let (inst_base, cell_base, block_base) = caller.emit_nexts().unwrap();
    caller.insts.reserve(callee.insts.len());
    caller
        .cells
        .reserve(callee.cells.len().saturating_sub(scratch.overrides.len()));
    scratch.cell_map.truncate(0);
    let mut skipped_cells = 0u32;
    for (cid, cell) in callee.cells.iter() {
        let mapped = if let Some(&mapped) = scratch.overrides.get(&cid) {
            skipped_cells += 1;
            mapped
        } else {
            let emit_id = cell_base + callee.emit_cell_id(cid) - skipped_cells;
            caller.alloc_cell_with_emit_id(cell.clone(), emit_id)
        };
        scratch.cell_map.insert(cid, mapped);
    }
    scratch.block_map.truncate(0);
    if kind == Kind::General {
        import_cleanup_load_cells(caller, callee, &scratch.cell_map);
        let forced = map_forced_cells(caller, callee, &scratch.overrides, cell_base, block_base);
        let ids = caller.emit_ids.as_mut().unwrap();
        for (cell, block) in forced {
            ids.forced_cells.push(cell);
            ids.forced_cell_blocks.push(block);
        }
        caller.blocks.reserve(callee.blocks.len());
        for (bid, _) in callee.blocks.iter() {
            let nbid = caller.alloc_block_with_emit_id(block_base + callee.emit_block_id(bid));
            scratch.block_map.insert(bid, nbid);
        }
    }
    let first_inst = caller.insts.len();
    let remap = |id| Idx::from_raw(((first_inst + raw(id)) as u32).into());
    for (iid, inst) in callee.insts.iter() {
        let mut clone = inst.clone();
        remap_clone(&mut clone, &remap, &scratch.cell_map, &scratch.block_map);
        caller.alloc_inst_with_emit_id(clone, inst_base + callee.emit_inst_id(iid));
    }
    scratch.inst_ids = caller.insts.allocated_since(first_inst);
    copy_projection_metadata(caller, callee, |id| Some(remap(id)));
    if kind == Kind::General {
        for (bid, block) in callee.blocks.iter() {
            caller.blocks[scratch.block_map[bid]] = remap_block(block, &remap, &scratch.block_map);
        }
        caller.for_iter_headers.extend(
            callee
                .for_iter_headers
                .iter()
                .map(|header| scratch.block_map[*header]),
        );
    }
    caller.set_emit_nexts((
        inst_base + callee.emit_inst_span(),
        cell_base + callee.emit_cell_span() - skipped_cells,
        if kind == Kind::General {
            block_base + callee.emit_block_span()
        } else {
            block_base
        },
    ));
}

fn import_cleanup_load_cells(
    caller: &mut MirFunc,
    callee: &MirFunc,
    cell_map: &ArenaMap<Idx<Cell>, Idx<Cell>>,
) {
    if callee.inline_cleanup_load_cells.is_empty() {
        return;
    }
    let mut existing: HashSet<Idx<Cell>> =
        caller.inline_cleanup_load_cells.iter().copied().collect();
    for &cell in &callee.inline_cleanup_load_cells {
        if let Some(&mapped) = cell_map.get(cell) {
            if existing.insert(mapped) {
                caller.inline_cleanup_load_cells.push(mapped);
            }
        }
    }
}

fn remap_clone(
    inst: &mut Inst,
    remap: &impl Fn(Idx<Inst>) -> Idx<Inst>,
    cm: &ArenaMap<Idx<Cell>, Idx<Cell>>,
    bm: &ArenaMap<Idx<Block>, Idx<Block>>,
) {
    inst.remap_uses(remap);
    match inst {
        Inst::Load(c) => *c = cm[*c],
        Inst::Store { cell, .. } => *cell = cm[*cell],
        Inst::CallMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. } => {
            for d in dests {
                *d = cm[*d];
            }
        }
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            *key_cell = cm[*key_cell];
            *val_cell = cm[*val_cell];
        }
        Inst::Lambda { params, entry } => {
            for p in params {
                *p = cm[*p];
            }
            *entry = bm[*entry];
        }
        _ => {}
    }
}

fn remap_block(
    block: &Block,
    remap: &impl Fn(Idx<Inst>) -> Idx<Inst>,
    bm: &ArenaMap<Idx<Block>, Idx<Block>>,
) -> Block {
    let mut term = block.term.clone();
    term.remap_uses(remap);
    term.remap_targets(&|x| bm[x]);
    Block {
        insts: block.insts.iter().copied().map(remap).collect(),
        term,
    }
}

pub(crate) fn clone_lambda_region(
    mf: &mut MirFunc,
    blocks: &[Idx<Block>],
    params: &[Idx<Cell>],
    captured: &HashSet<Idx<Cell>>,
) -> (Vec<Idx<Block>>, Vec<Idx<Cell>>) {
    let mut cell_map = ArenaMap::default();
    let mut block_map = ArenaMap::default();
    let mut inst_map = HashMap::default();
    let mut cells = params.to_vec();
    for &b in blocks {
        block_map.insert(b, mf.new_block());
        for id in mf.blocks[b].insts.clone() {
            let inst = mf.insts[id].clone();
            inst.cells_each(&mut |cell| cells.push(cell));
            inst_map.insert(id, mf.alloc_inst(inst));
        }
    }
    cells.sort_unstable();
    cells.dedup();
    for cell in cells {
        let mapped = if captured.contains(&cell) {
            cell
        } else {
            let mapped = mf.new_cell(None);
            mf.cells[mapped] = mf.cells[cell].clone();
            mapped
        };
        cell_map.insert(cell, mapped);
    }
    for (&old, &new) in &inst_map {
        remap_clone(
            &mut mf.insts[new],
            &|id| inst_map[&id],
            &cell_map,
            &block_map,
        );
        macro_rules! copy_maps {
            ($($map:ident),+ $(,)?) => {$(
                if let Some(value) = mf.$map.get(&old).cloned() {
                    mf.$map.insert(new, value);
                }
            )+};
        }
        copy_maps!(
            bound_trees,
            resolved_bound_trees,
            spec_bindings,
            callee_instances,
            call_abi_plans,
            call_rep_plans,
            flat_call_lanes,
            flat_box_widths,
            call_sites,
            fn_value_symbols,
            code_value_identities,
        );
        if let Some(&span) = mf.inst_spans.get(old) {
            mf.inst_spans.insert(new, span);
        }
        macro_rules! copy_sets {
            ($($set:ident),+ $(,)?) => {$(
                if mf.$set.contains(&old) {
                    mf.$set.insert(new);
                }
            )+};
        }
        copy_sets!(
            binding_initializers,
            bound_next_split,
            materialized_default_guards,
            synthetic_projections,
        );
    }
    for &b in blocks {
        mf.blocks[block_map[b]] = remap_block(&mf.blocks[b], &|id| inst_map[&id], &block_map);
    }
    let headers = mf
        .for_iter_headers
        .iter()
        .filter_map(|&b| block_map.get(b).copied())
        .collect::<Vec<_>>();
    mf.for_iter_headers.extend(headers);
    (
        blocks.iter().map(|&block| block_map[block]).collect(),
        params.iter().map(|&param| cell_map[param]).collect(),
    )
}

/// Coalesce only if the argument cell still holds the value its load read.
fn coalesce_overrides(
    caller: &MirFunc,
    cand: &CalleeBody,
    args: &[Idx<Inst>],
    pos: (Idx<Block>, usize),
    pinned: &HashSet<Idx<Cell>>,
    scratch: &mut InlineScratch,
) {
    let callee = &cand.func;
    scratch.overrides.clear();
    scratch.coalesced.clear();
    scratch.coalesced.resize(callee.params.len(), false);
    for (i, &load) in args.iter().enumerate().take(callee.params.len()) {
        if !cand.coalescable_params.get(i).copied().unwrap_or(false) {
            continue;
        }
        // Coalescing a converted load would erase its numeric conversion.
        let Inst::Load(c) = caller.insts[load] else {
            continue;
        };
        if snapshot_holds(caller, pinned, &mut scratch.predecessors, c, load, pos) {
            scratch.overrides.insert(callee.params[i], c);
            scratch.coalesced[i] = true;
        }
    }
}

/// A copied argument is a snapshot until the call and throughout the splice.
fn snapshot_holds(
    caller: &MirFunc,
    pinned: &HashSet<Idx<Cell>>,
    preds: &mut Option<Predecessors>,
    c: Idx<Cell>,
    load: Idx<Inst>,
    (bid, idx): (Idx<Block>, usize),
) -> bool {
    if pinned.contains(&c) {
        return false;
    }
    let clear = |insts: &[Idx<Inst>]| {
        insts.iter().all(|&i| {
            let mut touches = false;
            if !matches!(caller.insts[i], Inst::Load(_)) {
                caller.insts[i].cells_each(&mut |cell| touches |= cell == c);
            }
            !touches
        })
    };
    let call_prefix = &caller.blocks[bid].insts[..idx];
    if let Some(pos) = call_prefix.iter().rposition(|&i| i == load) {
        return clear(&call_prefix[pos + 1..]);
    }
    if !clear(call_prefix) {
        return false;
    }
    // The load dominates the call, so all intervening paths must keep the value.
    let preds = preds.get_or_insert_with(|| {
        let mut map = vec![SmallVec::new(); caller.blocks.len()];
        for (from, block) in caller.blocks.iter() {
            skull_mir::cfg::successors(&block.term, |to| map[raw(to)].push(from));
        }
        map
    });
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    let mut stack = preds[raw(bid)].clone();
    let mut found = false;
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        let insts = &caller.blocks[b].insts;
        if let Some(pos) = insts.iter().position(|&i| i == load) {
            found = true;
            if !clear(&insts[pos + 1..]) {
                return false;
            }
            continue;
        }
        if !clear(insts) {
            return false;
        }
        stack.extend_from_slice(&preds[raw(b)]);
    }
    found
}

fn pin_cells(func: &MirFunc, top: &[bool], blocks_from: usize, pinned: &mut HashSet<Idx<Cell>>) {
    for (bid, block) in func.blocks.iter().skip(blocks_from) {
        if top[raw(bid)] {
            continue;
        }
        for &iid in &block.insts {
            if !matches!(func.insts[iid], Inst::Load(_)) {
                func.insts[iid].cells_each(&mut |cell| {
                    pinned.insert(cell);
                });
            }
        }
    }
}

fn graft_resolved_bound_tree(
    tree: &skull_mir::bound::ResolvedBoundTree,
    incoming: &skull_mir::bound::ResolvedBoundTree,
) -> Result<skull_mir::bound::ResolvedBoundTree, CompilerError> {
    let mut out = skull_mir::bound::ResolvedBoundTree::default();
    for (&g, next) in &tree.next {
        let next = match next {
            skull_mir::bound::ResolvedBoundNext::Concrete { instance, sub } => {
                skull_mir::bound::ResolvedBoundNext::Concrete {
                    instance: instance.clone(),
                    sub: graft_resolved_bound_tree(sub, incoming)?,
                }
            }
            skull_mir::bound::ResolvedBoundNext::CallerGeneric(g) => {
                incoming.get(*g).cloned().ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "internal: unresolved concrete `Iterator` bound (generic #{g}) while inlining"
                    ))
                })?
            }
        };
        out.next.insert(g, next);
    }
    Ok(out)
}

/// Resolve cloned placeholders using the call site's witness tree.
fn resolve_cloned_bound_calls(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    site_tree: Option<&BoundTree>,
    site_resolved_tree: Option<&skull_mir::bound::ResolvedBoundTree>,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let empty = BoundTree::default();
    let n = site_tree.unwrap_or(&empty);
    let incoming = match site_resolved_tree {
        Some(tree) => tree.clone(),
        None if n.is_empty() => Default::default(),
        None => {
            return Err(CompilerError::new_no_location(
                "internal: concrete splice site has no resolved bound plan",
            ));
        }
    };
    let mut resolved_by_old: HashMap<Idx<Inst>, skull_mir::bound::ResolvedBoundTree> =
        HashMap::default();

    for old_id in callee.bound_trees.keys() {
        let new_id = inst_map[old_id];
        let resolved = callee.resolved_bound_trees.get(old_id).ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "internal: concrete callee `{}` has a raw bound tree without its resolved plan",
                callee.name
            ))
        })?;
        let grafted = graft_resolved_bound_tree(resolved, &incoming)?;
        if !grafted.is_empty() {
            func.resolved_bound_trees.insert(new_id, grafted.clone());
            resolved_by_old.insert(*old_id, grafted);
        }
    }

    for (&old_id, &new_id) in inst_map {
        let Inst::CallBoundMulti { generic_idx, .. } = &func.insts[new_id] else {
            continue;
        };
        debug_assert!(matches!(callee.insts[old_id], Inst::CallBoundMulti { .. }));
        let g = *generic_idx as usize;
        let tree = resolved_by_old.get(&old_id).unwrap_or(&incoming);
        let Some(leaf) = tree.next.get(&g).cloned() else {
            return Err(CompilerError::new_no_location(format!(
                "internal: placeholder bound-call (generic #{g}) has no concrete \
                 resolution at this splice site"
            )));
        };
        if let skull_mir::bound::ResolvedBoundNext::Concrete { sub, .. } = &leaf {
            if !sub.is_empty() {
                func.resolved_bound_trees.insert(new_id, sub.clone());
            }
        }
        crate::bound::substitute_bound_call(
            func,
            new_id,
            callee.bound_next_split.contains(&old_id),
            &leaf,
            scx,
            ws,
            concrete_plans,
            "at this splice site",
        )?;
    }
    if let Some((&old_id, &new_id)) = inst_map
        .iter()
        .find(|(_, new_id)| matches!(func.insts[**new_id], Inst::CallBoundMulti { .. }))
    {
        return Err(CompilerError::new_no_location(format!(
            "internal: cloned bounded placeholder {:?} from `{}` survived concrete splice as {:?}",
            old_id, callee.name, new_id
        )));
    }
    Ok(())
}

fn collect_nested_names(nested: &[MirFunc], out: &mut Vec<Ustr>) {
    for nf in nested {
        out.push(nf.name);
        collect_nested_names(&nf.nested_fns, out);
    }
}

fn remap_nested_global(inst: &mut Inst, names: &HashMap<Ustr, Ustr>) {
    match inst {
        Inst::Global(name)
        | Inst::MakeEnv {
            fn_ref: Some(name), ..
        } => {
            if let Some(&new) = names.get(name) {
                *name = new;
            }
        }
        _ => {}
    }
}

fn rename_nested_tree(nested: &mut [MirFunc], names: &HashMap<Ustr, Ustr>) {
    for nf in nested {
        if let Some(&new) = names.get(&nf.name) {
            nf.name = new;
        }
        for (_, inst) in nf.insts.iter_mut() {
            remap_nested_global(inst, names);
        }
        nf.nested_fn_identities = nf
            .nested_fn_identities
            .iter()
            .map(|(name, identity)| (names.get(name).copied().unwrap_or(*name), identity.clone()))
            .collect();
        nf.hoisted_fn_globals = nf
            .hoisted_fn_globals
            .iter()
            .map(|name| names.get(name).copied().unwrap_or(*name))
            .collect();
        rename_nested_tree(&mut nf.nested_fns, names);
    }
}

fn import_nested_fns(
    func: &mut MirFunc,
    callee: &MirFunc,
    insts: IdxRange<Inst>,
) -> HashMap<Ustr, Ustr> {
    if callee.nested_fns.is_empty() {
        return HashMap::default();
    }
    let mut old_names = Vec::new();
    collect_nested_names(&callee.nested_fns, &mut old_names);
    let mut used = Vec::new();
    collect_nested_names(&func.nested_fns, &mut used);
    used.extend(func.hoisted_fn_globals.iter().copied());
    used.extend(old_names.iter().copied());
    let mut used: HashSet<Ustr> = used.into_iter().collect();
    let mut names = HashMap::default();
    let mut nonce = func.nested_fns.len();
    for old in old_names {
        if names.contains_key(&old) {
            continue;
        }
        loop {
            let new = Ustr::from(format!("{old}__inline{nonce}").as_str());
            nonce += 1;
            if used.insert(new) {
                names.insert(old, new);
                break;
            }
        }
    }
    for new_id in insts {
        remap_nested_global(&mut func.insts[new_id], &names);
    }
    let mut nested = callee.nested_fns.clone();
    rename_nested_tree(&mut nested, &names);
    func.nested_fns.extend(nested);
    names
}

fn import_splice_metadata(
    func: &mut MirFunc,
    callee: &MirFunc,
    mapped: impl Fn(Idx<Inst>) -> Option<Idx<Inst>> + Copy,
    nested_names: &HashMap<Ustr, Ustr>,
) {
    copy_inst_metadata(func, callee, mapped);
    copy_inst_tables!(
        func,
        callee,
        mapped,
        resolved_bound_trees,
        closure_env_plans
    );
    for (name, identity) in &callee.nested_fn_identities {
        let name = nested_names.get(name).copied().unwrap_or(*name);
        func.nested_fn_identities.insert(name, identity.clone());
    }
    for n in &callee.hoisted_fn_globals {
        let n = nested_names.get(n).copied().unwrap_or(*n);
        if !func.hoisted_fn_globals.contains(&n) {
            func.hoisted_fn_globals.push(n);
        }
    }
    import_call_sites(func, callee, mapped, None);
}

fn import_call_sites(
    target: &mut MirFunc,
    source: &MirFunc,
    mapped: impl Fn(Idx<Inst>) -> Option<Idx<Inst>> + Copy,
    missing_owner: Option<&str>,
) {
    // The site keeps its source expression arena when its owner changes tables.
    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, cs) in &source.call_sites {
        let Some(new_id) = mapped(*old_id) else {
            continue;
        };
        let owner = match owner_map.get(&cs.owner.0) {
            Some(&o) => o,
            None => {
                let Some(inst) = source.site_owners.get(cs.owner.0 as usize) else {
                    if let Some(message) = missing_owner {
                        panic!("{message}");
                    }
                    continue;
                };
                let o = skull_mir::site::intern_owner(&mut target.site_owners, inst.clone());
                owner_map.insert(cs.owner.0, o);
                o
            }
        };
        target.call_sites.insert(
            new_id,
            skull_mir::site::CallSite {
                owner,
                site: cs.site.clone(),
            },
        );
    }
}

fn splice(
    func: &mut MirFunc,
    site: Site,
    cand: &CalleeBody,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
    pinned: &HashSet<Idx<Cell>>,
    scratch: &mut InlineScratch,
    aliases: &mut ResultAliases,
) -> Result<(), CompilerError> {
    scratch.added_blocks.clear();
    let callee = &cand.func;
    let (bid, idx) = site.pos;
    let args = &site.args;
    match &mut func.insts[site.call_id] {
        Inst::Call { args: original, .. } | Inst::CallMulti { args: original, .. } => {
            original.copy_from_slice(args);
        }
        _ => unreachable!("inline site must be a concrete call"),
    }
    let site_tree = func.bound_trees.remove(&site.call_id);
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id);
    coalesce_overrides(func, cand, args, (bid, idx), pinned, scratch);
    clone_body(func, callee, scratch, cand.kind);
    // Bound repacking allocates instructions in the original map's order.
    let bound_inst_map = &mut scratch.bound_inst_map;
    bound_inst_map.clear();
    bound_inst_map.reserve(callee.insts.len());
    if cand.resolves_bounds || site_tree.as_ref().is_some_and(|tree| !tree.is_empty()) {
        for ((old, _), new) in callee.insts.iter().zip(scratch.inst_ids) {
            bound_inst_map.insert(old, new);
        }
        resolve_cloned_bound_calls(
            func,
            callee,
            bound_inst_map,
            site_tree.as_ref(),
            site_resolved_tree.as_ref(),
            ws,
            scx,
            concrete_plans,
        )?;
    }
    let names = import_nested_fns(func, callee, scratch.inst_ids);
    let mapped = |id| Some(splice_inst(scratch.inst_ids, id));
    import_splice_metadata(func, callee, mapped, &names);
    let forwarded = forward_param_reads(func, cand, args, scratch);
    if cand.kind == Kind::General && !forwarded.is_empty() {
        for source in &cand.reachable {
            func.blocks[scratch.block_map[*source]]
                .insts
                .retain(|inst| !forwarded.iter().any(|(read, _)| read == inst));
        }
    }
    match cand.kind {
        Kind::Straight => splice_straight(func, &site, cand, scratch, aliases, &forwarded),
        Kind::General => splice_general(func, &site, cand, scratch),
    }
    Ok(())
}

fn bind_params(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &InlineScratch,
    push: &mut impl FnMut(&mut MirFunc, Idx<Inst>),
) {
    let args = &site.args;
    for (i, (param, &arg)) in cand.func.params.iter().zip(args).enumerate() {
        if scratch.coalesced[i] {
            continue;
        }
        let store = func.alloc_inst(Inst::Store {
            cell: scratch.cell_map[*param],
            value: arg,
        });
        if !copy_unread(cand, i) || !forwards_arg(func, arg) {
            push(func, store);
        }
    }
}

fn splice_straight(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &mut InlineScratch,
    aliases: &mut ResultAliases,
    forwarded: &[(Idx<Inst>, Idx<Inst>)],
) {
    let callee = &cand.func;
    let (bid, idx) = site.pos;
    let block = &callee.blocks[callee.entry];
    let mut replacement = std::mem::take(&mut scratch.replacement);
    bind_params(func, site, cand, scratch, &mut |_, inst| {
        replacement.push(inst);
    });
    replacement.extend(
        block
            .insts
            .iter()
            .map(|&inst| splice_inst(scratch.inst_ids, inst))
            .filter(|inst| !forwarded.iter().any(|(read, _)| read == inst)),
    );

    let Term::Return(values) = &block.term else {
        unreachable!("straight candidate entry must end in Return");
    };
    let mapped = |value: &Idx<Inst>| forwarded_to(forwarded, splice_inst(scratch.inst_ids, *value));
    let mut result = None;
    if let Some(dests) = &site.dests {
        for (&cell, value) in dests.iter().zip(values) {
            let st = func.alloc_inst(Inst::Store {
                cell,
                value: mapped(value),
            });
            replacement.push(st);
        }
    } else if let Some(value) = values.first() {
        result = Some(mapped(value));
    } else {
        func.insts[site.call_id] = Inst::Const(Lit::Nil);
        replacement.push(site.call_id);
    }

    func.blocks[bid]
        .insts
        .splice(idx..=idx, replacement.drain(..));
    scratch.replacement = replacement;
    if let Some(to) = result.filter(|&to| to != site.call_id) {
        aliases.values.insert(site.call_id, to);
    }
}

fn splice_general(func: &mut MirFunc, site: &Site, cand: &CalleeBody, scratch: &mut InlineScratch) {
    let (bid, idx) = site.pos;
    let callee = &cand.func;
    let entry_clone = scratch.block_map[callee.entry];
    scratch.predecessors = None;

    let after = func.blocks[bid].insts.split_off(idx + 1);
    func.blocks[bid].insts.truncate(idx);
    let orig_term = std::mem::replace(&mut func.blocks[bid].term, Term::Goto(entry_clone));

    let cont = func.new_block();
    func.blocks[cont].insts = after;
    func.blocks[cont].term = orig_term;

    let result_cell = match &site.dests {
        None => {
            let call_id = site.call_id;
            let rc = func.new_cell(None);
            func.insts[call_id] = Inst::Load(rc);
            func.blocks[cont].insts.insert(0, call_id);
            Some(rc)
        }
        Some(_) => None,
    };

    bind_params(func, site, cand, scratch, &mut |func, inst| {
        func.blocks[bid].insts.push(inst);
    });

    // Only the callee CFG returns to the caller. Lambda returns stay in their sub-CFGs.
    for &source_block in &cand.reachable {
        let nb = scratch.block_map[source_block];
        let vs = match &func.blocks[nb].term {
            Term::Return(vs) => vs.clone(),
            _ => continue,
        };
        match &site.dests {
            None => {
                let rc = result_cell.unwrap();
                let v = match vs.first() {
                    Some(v) => *v,
                    None => {
                        let nil = func.alloc_inst(Inst::Const(Lit::Nil));
                        func.blocks[nb].insts.push(nil);
                        nil
                    }
                };
                let st = func.alloc_inst(Inst::Store { cell: rc, value: v });
                func.blocks[nb].insts.push(st);
            }
            Some(dests) => {
                for (dest, rv) in dests.iter().zip(vs.iter()) {
                    let st = func.alloc_inst(Inst::Store {
                        cell: *dest,
                        value: *rv,
                    });
                    func.blocks[nb].insts.push(st);
                }
            }
        }
        func.blocks[nb].term = Term::Goto(cont);
    }
    scratch
        .added_blocks
        .extend(cand.reachable.iter().map(|b| scratch.block_map[*b]));
    scratch.added_blocks.push(cont);
}

#[derive(Default)]
pub(crate) struct InlineOutcome {
    pub dependencies: HashSet<FuncRef>,
    pub callbacks: HashSet<Idx<Inst>>,
    pub splices: usize,
    pub copied_instructions: usize,
}

pub(crate) fn inline_functions_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<InlineOutcome, CompilerError> {
    let mut outcome = InlineOutcome::default();
    if inline_off_active() {
        return Ok(outcome);
    }
    let mut top = vec![false; func.blocks.len()];
    for block in top_level_blocks(func) {
        top[raw(block)] = true;
    }
    let mut pinned: HashSet<Idx<Cell>> = HashSet::default();
    pin_cells(func, &top, 0, &mut pinned);
    // The predecessor table stays valid across straight-line splices.
    cache.scratch.predecessors = None;
    let mut aliases = ResultAliases::default();
    let mut resume: Option<(Idx<Block>, usize)> = None;
    let mut splice_chains: HashMap<Idx<Inst>, SpliceChain> = HashMap::default();
    let mut budget = MAX_INLINES_PER_FN;
    let mut callbacks = CallbackProof::default();
    let noalloc_caller = ws.module(self_ref.module).functions[self_ref.func]
        .annotations
        .iter()
        .any(|a| a.name.name.as_str() == "noalloc");
    while budget > 0 {
        let Some(site) = find_site(
            func,
            ws,
            scx,
            cache,
            self_ref,
            noalloc_caller,
            &top,
            resume,
            &splice_chains,
            concrete_plans,
            &mut callbacks,
            &aliases,
        )?
        else {
            break;
        };
        let pos = site.pos;
        let fref = site.inst.func;
        let mut chain = splice_chains.remove(&site.call_id).unwrap_or_default();
        chain.push(fref);
        let cand = cache.cands[&site.inst]
            .as_deref()
            .or_else(|| {
                (!site.callbacks.is_empty())
                    .then(|| cache.callback_cands[&site.inst].as_deref())
                    .flatten()
            })
            .expect("site implies a cached candidate");
        let scratch = &mut cache.scratch;
        let blocks_before = func.blocks.len();
        outcome.callbacks.extend(site.callbacks.iter().copied());
        splice(
            func,
            site,
            cand,
            ws,
            scx,
            concrete_plans,
            &pinned,
            scratch,
            &mut aliases,
        )?;
        top.resize(func.blocks.len(), false);
        for block in scratch.added_blocks.drain(..) {
            top[raw(block)] = true;
        }
        pin_cells(func, &top, blocks_before, &mut pinned);
        for inst in scratch.inst_ids {
            if matches!(func.insts[inst], Inst::Call { .. } | Inst::CallMulti { .. }) {
                splice_chains.insert(inst, chain.clone());
            }
        }
        outcome.copied_instructions += cand.insts;
        func.statics.extend(cand.func.statics.iter().cloned());
        func.comptime_reads
            .extend(cand.func.comptime_reads.iter().cloned());
        func.struct_registry_roots
            .extend(cand.func.struct_registry_roots.iter().copied());
        outcome.dependencies.insert(fref);
        resume = Some(pos);
        budget -= 1;
    }
    if func.statics.len() > 1 {
        let mut seen: HashSet<ustr::Ustr> = HashSet::default();
        func.statics.retain(|sc| seen.insert(sc.cell));
    }
    aliases.finish(func);
    outcome.dependencies.extend(callbacks.dependencies);
    outcome.splices = MAX_INLINES_PER_FN - budget;
    Ok(outcome)
}

fn fn_has_bounds(f: &bone_core::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

// Walk block inst-lists, not the raw arena: a splice drops the consumed call from
// its block but the arena still holds it.
pub fn validate_no_residual_bound_calls(
    ws: &Workspace,
    f: &bone_core::hir::Function,
    mf: &MirFunc,
) -> Result<(), CompilerError> {
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            match &mf.insts[iid] {
                Inst::CallBoundMulti { .. } => {
                    return Err(CompilerError::new_no_location(format!(
                        "`{}`: an `Iterator`-bounded call retained unresolved placeholders \
                         after neither concrete splicing nor standalone planning (is this a \
                         recursive bounded fn?)",
                        f.name
                    )));
                }
                Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                    let callee = &ws.module(func.module).functions[func.func];
                    // A non-empty concrete owner is the admission: a bounded
                    // call without one would have no resolved standalone body.
                    let named = mf
                        .spec_bindings
                        .get(&iid)
                        .is_some_and(|i| !i.args.is_empty());
                    if fn_has_bounds(callee) && !named {
                        return Err(CompilerError::new_no_location(format!(
                            "`{}` calls `Iterator`-bounded `{}` in a context that cannot inline \
                             it, and the call was not specialized either -- so nothing serves \
                             it. A bounded body IS emitted standalone when a call demands one \
                             (plan S 12); what is missing here is the demand, which the lowerer \
                             raises only for an instantiation it specializes. Iterator \
                             combinators inside `co fn` bodies and recursive bounded fns are \
                             still the cases that hit this.",
                            f.name, callee.name
                        )));
                    }
                }
                _ => {}
            }
        }
    }
    for nested in &mf.nested_fns {
        validate_no_residual_bound_calls(ws, f, nested)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ustr::Ustr;

    #[test]
    fn splice_rewires_arguments_and_preserves_bound_element_repack() {
        let src = r#"
nobox value Pair { x: int, y: int }
struct Counter { n: int }
fn next(c: Counter) -> (bool, int) {
    if c.n >= 3 { return (false, 0) }
    c.n = c.n + 1
    return (true, c.n)
}
@noalloc
fn wide_next(c: Counter) -> (bool, Pair) {
    return (true, Pair { x: c.n, y: c.n })
}
fn sum_all<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for x in it { s = s + 1 }
    return s
}
@inline
fn counter() -> Counter { return Counter { n: 0 } }
fn top() -> int { return sum_all(counter()) + counter().n }
"#;
        let mut modules: Vec<_> = rock_stdlib::stdlib_files()
            .iter()
            .map(|file| {
                let id = Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str());
                skull_parser::parse_module_clean(file.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(
            skull_parser::parse_module_clean(src, Ustr::from("<repack-index>")).expect("parse"),
        );
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<repack-index>"))
            .expect("module");
        let find = |name| FuncRef {
            module: mid,
            func: ws
                .module(mid)
                .functions
                .iter()
                .find(|(_, f)| skull_mir::mangle::declared_name(f.name.as_str()) == name)
                .expect("fixture function")
                .0,
        };
        let (top, wide, counter) = (find("top"), find("wide_next"), find("counter"));
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let graph = skull_graph::instance_graph::build_in(
            &ws,
            vec![top, wide],
            std::sync::Arc::clone(&interner),
        );
        let concrete = skull_graph::concrete_body::build(&ws, &graph);
        let scx = ScalarCtx::new(&ws).sharing(interner);
        let plans = &concrete.plans;
        let plan = plans.get(&Instance::base(top)).expect("top plan");
        let mut func =
            skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, plans).expect("lower top");
        let counter_calls: Vec<_> = func
            .insts
            .iter()
            .filter_map(|(id, inst)| {
                matches!(inst, Inst::Call { func, .. } if *func == counter).then_some(id)
            })
            .collect();
        assert_eq!(counter_calls.len(), 2);
        // Give the splice a witness with a wider result than the body planned for.
        let witness = func
            .resolved_bound_trees
            .values_mut()
            .find_map(|tree| tree.next.get_mut(&0))
            .expect("site witness");
        let skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. } = witness else {
            panic!("site witness must be concrete");
        };
        assert_eq!(*instance, Instance::base(find("next")));
        *instance = Instance::base(wide);

        let mut cache = InlineCache::default();
        inline_functions_with_plans(&ws, &scx, top, &mut func, &mut cache, plans)
            .expect("inline top");

        let mut uses_counter_call = false;
        for (_, block) in func.blocks.iter() {
            let mut check = |value| uses_counter_call |= counter_calls.contains(&value);
            for &iid in &block.insts {
                assert!(!counter_calls.contains(&iid));
                func.insts[iid].uses_each(&mut check);
            }
            block.term.uses_each(&mut check);
        }
        assert!(!uses_counter_call, "the removed call still has live users");

        let repacked = func.blocks.iter().any(|(_, block)| {
            let Some((call, dests)) =
                block
                    .insts
                    .iter()
                    .enumerate()
                    .find_map(|(at, &id)| match &func.insts[id] {
                        Inst::CallMulti { func, dests, .. } if *func == wide => Some((at, dests)),
                        _ => None,
                    })
            else {
                return false;
            };
            block.insts[call + 1..].iter().any(|&id| {
                let Inst::MakeValueStruct {
                    form: ValueCtorForm::Leaves,
                    fields,
                    ..
                } = &func.insts[id]
                else {
                    return false;
                };
                fields.len() == dests.len() - 1
                    && fields.iter().zip(&dests[1..]).all(|((_, value), dest)| {
                        matches!(func.insts[*value], Inst::Load(cell) if cell == *dest)
                            && block.insts[call + 1..].contains(value)
                    })
            })
        });
        assert!(
            repacked,
            "the repack must load each leaf returned by the bound call in order"
        );
    }
}
