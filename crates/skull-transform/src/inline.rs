//! Splice eligible direct calls and record the concrete bodies embedded in each caller.
//! Annotation gating limits the number of chunks a body edit invalidates.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smallvec::SmallVec;
use std::cell::Cell as ThreadCell;
use std::sync::Arc;

use bone_core::prelude::{BoundTree, CompilerError};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BackendFnAbiPlanHandle, ScalarCtx};
use skull_mir::mir::*;
use ustr::Ustr;

use crate::top_level_blocks;

const MAX_INLINES_PER_FN: usize = 1000;
/// Bound expansion even when register allocation can coalesce most cells.
const CELL_BUDGET: usize = 400;

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
    INLINE_OFF.with(|off| off.get())
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
    /// The raw lowered body shared with emission consumers. Every mutating
    /// consumer clones it; only this table owns the canonical source.
    canonical: Arc<MirFunc>,
    /// The concrete body this snapshot was lowered from. A declaration-level
    /// snapshot is not a valid source for a concrete splice.
    subject: Instance,
    abi: BackendFnAbiPlanHandle,
    kind: Kind,
    ret_arity: usize,
    /// Blocks the CFG splice adds to the caller's reachable set.
    reachable: Vec<Idx<Block>>,
    insts: usize,
    source_cells: usize,
    /// Coalescing preserves field mutation through a shared argument. A
    /// parameter assigned as a whole needs its own cell.
    coalescable_params: Vec<bool>,
    /// A lambda sub-CFG reads captured cells, so its body cannot forward these reads.
    forwardable_reads: Vec<(usize, Idx<Inst>)>,
    /// Whether `forwardable_reads` covers every read of a coalescable parameter.
    forwards_reads: bool,
}

impl CalleeBody {
    /// Cells a splice charges to the caller's budget: the callee's cells and
    /// the `let`s it holds as values, which a cell would have held before.
    fn budget_cells(&self) -> usize {
        self.source_cells + self.func.value_lets
    }
}

pub struct SpliceRecord {
    pub callee: FuncRef,
    pub callee_insts: usize,
}

/// Cache one result per concrete instantiation, including rejected candidates.
type CacheKey = Instance;

pub struct InlineCandidateTable {
    entries: HashMap<CacheKey, Option<Arc<CalleeBody>>>,
    removed: HashSet<CacheKey>,
    parent: Option<Arc<InlineCandidateTable>>,
}

pub struct InlineCandidateWarm {
    key: CacheKey,
    candidate: Option<Arc<CalleeBody>>,
}

impl InlineCandidateTable {
    pub fn from_warms_with_base(
        base: Option<&Self>,
        mut keys: Vec<CacheKey>,
        warms: Vec<InlineCandidateWarm>,
    ) -> Self {
        keys.sort();
        keys.dedup();
        let mut warmed = HashMap::default();
        warmed.reserve(warms.len());
        for warm in warms {
            let previous = warmed.insert(warm.key, warm.candidate);
            assert!(
                previous.is_none(),
                "inline candidate table received a duplicate key"
            );
        }

        let mut entries = HashMap::default();
        entries.reserve(keys.len());
        for key in keys {
            if let Some(candidate) = warmed.remove(&key) {
                entries.insert(key, candidate);
                continue;
            }
            let candidate = base
                .and_then(|table| table.lookup(&key))
                .expect("inline candidate table omitted an unwarmed key");
            entries.insert(key, candidate);
        }
        assert!(
            warmed.is_empty(),
            "inline candidate table warmed an unused key"
        );
        Self {
            entries,
            removed: HashSet::default(),
            parent: None,
        }
    }

    pub fn from_warms_incremental(
        base: Arc<Self>,
        warms: Vec<InlineCandidateWarm>,
        removed: &[Instance],
    ) -> Self {
        let mut entries = HashMap::default();
        for warm in warms {
            let previous = entries.insert(warm.key, warm.candidate);
            assert!(
                previous.is_none(),
                "inline candidate delta received a duplicate key"
            );
        }
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
        let (Some(Some(before)), Some(Some(after))) = (self.lookup(key), other.lookup(key)) else {
            return false;
        };

        before.subject == after.subject
            && before.abi == after.abi
            && before.func.params.len() == after.func.params.len()
            && before.ret_arity == after.ret_arity
            && before.budget_cells() == after.budget_cells()
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

    pub fn canonical_body(&self, key: &Instance) -> Option<Arc<MirFunc>> {
        self.lookup(key)
            .and_then(|candidate| candidate)
            .map(|candidate| Arc::clone(&candidate.canonical))
    }

    fn lookup(&self, key: &CacheKey) -> Option<Option<Arc<CalleeBody>>> {
        if let Some(candidate) = self.entries.get(key) {
            return Some(candidate.clone());
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
    shared: Option<Arc<InlineCandidateTable>>,
    scratch: InlineScratch,
}

#[derive(Clone, Default)]
struct InlineScratch {
    inst_map: HashMap<Idx<Inst>, Idx<Inst>>,
    cell_map: HashMap<Idx<Cell>, Idx<Cell>>,
    block_map: HashMap<Idx<Block>, Idx<Block>>,
    overrides: HashMap<Idx<Cell>, Idx<Cell>>,
    coalesced: Vec<bool>,
    inst_ids: Vec<Idx<Inst>>,
    added_blocks: Vec<Idx<Block>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum UseSite {
    Inst(Idx<Inst>),
    Term(Idx<Block>),
}

/// Index uses so a splice can bind its result without scanning the caller.
#[derive(Default)]
struct UseIndex {
    users: Vec<SmallVec<[UseSite; 2]>>,
}

#[cfg(test)]
thread_local! {
    /// The last inlined caller's maintained index, diffed against a fresh
    /// build: `(missing, stale)` value-use pairs, as debug strings.
    static USE_INDEX_DRIFT: std::cell::RefCell<(Vec<String>, Vec<String>)> =
        Default::default();
}

impl UseIndex {
    #[cfg(test)]
    fn record_drift(&self, func: &MirFunc) {
        let pairs = |index: &UseIndex| -> HashSet<(Idx<Inst>, UseSite)> {
            index
                .users
                .iter()
                .enumerate()
                .flat_map(|(value, sites)| {
                    let value =
                        Idx::<Inst>::from_raw(comfy_la_arena::RawIdx::from_u32(value as u32));
                    sites.iter().map(move |site| (value, *site))
                })
                .collect()
        };
        let describe = |(value, site): &(Idx<Inst>, UseSite)| match site {
            UseSite::Inst(iid) => format!("{value:?} used by {iid:?} {:?}", func.insts[*iid]),
            UseSite::Term(bid) => format!("{value:?} used by term of {bid:?}"),
        };
        let fresh = pairs(&UseIndex::build(func));
        let kept = pairs(self);
        let mut missing: Vec<String> = fresh.difference(&kept).map(describe).collect();
        let mut stale: Vec<String> = kept.difference(&fresh).map(describe).collect();
        missing.sort();
        stale.sort();
        USE_INDEX_DRIFT.with(|drift| *drift.borrow_mut() = (missing, stale));
    }

    fn build(func: &MirFunc) -> Self {
        let mut index = Self::default();
        for (bid, block) in func.blocks.iter() {
            for &iid in &block.insts {
                index.add_inst(func, iid);
            }
            index.add_term(func, bid);
        }
        index
    }

    fn slot(&mut self, value: Idx<Inst>) -> &mut SmallVec<[UseSite; 2]> {
        let at = crate::visit::raw(value);
        if self.users.len() <= at {
            self.users.resize_with(at + 1, SmallVec::new);
        }
        &mut self.users[at]
    }

    fn add_use(&mut self, value: Idx<Inst>, site: UseSite) {
        let users = self.slot(value);
        if !users.contains(&site) {
            users.push(site);
        }
    }

    fn remove_use(&mut self, value: Idx<Inst>, site: UseSite) {
        if let Some(users) = self.users.get_mut(crate::visit::raw(value)) {
            users.retain(|user| *user != site);
        }
    }

    fn add_inst(&mut self, func: &MirFunc, iid: Idx<Inst>) {
        func.insts[iid].uses_each(&mut |value| {
            self.add_use(value, UseSite::Inst(iid));
        });
    }

    fn add_insts_from(&mut self, func: &MirFunc, mark: usize) {
        for (iid, _) in func.insts.iter().skip(mark) {
            self.add_inst(func, iid);
        }
    }

    fn remove_inst(&mut self, func: &MirFunc, iid: Idx<Inst>) {
        func.insts[iid].uses_each(&mut |value| {
            self.remove_use(value, UseSite::Inst(iid));
        });
    }

    fn add_term(&mut self, func: &MirFunc, bid: Idx<Block>) {
        func.blocks[bid].term.uses_each(&mut |value| {
            self.add_use(value, UseSite::Term(bid));
        });
    }

    fn remove_term(&mut self, func: &MirFunc, bid: Idx<Block>) {
        func.blocks[bid].term.uses_each(&mut |value| {
            self.remove_use(value, UseSite::Term(bid));
        });
    }

    fn replace(&mut self, func: &mut MirFunc, from: Idx<Inst>, to: Idx<Inst>) {
        if from == to {
            return;
        }
        let remap = |value| if value == from { to } else { value };
        for site in std::mem::take(self.slot(from)) {
            match site {
                UseSite::Inst(iid) => {
                    self.remove_inst(func, iid);
                    func.insts[iid].remap_uses(&remap);
                    self.add_inst(func, iid);
                }
                UseSite::Term(bid) => {
                    self.remove_term(func, bid);
                    func.blocks[bid].term.remap_uses(&remap);
                    self.add_term(func, bid);
                }
            }
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

    pub fn canonical_body(&self, key: &Instance) -> Option<Arc<MirFunc>> {
        self.shared
            .as_ref()
            .and_then(|table| table.canonical_body(key))
    }

    pub fn replace_candidate_table(&mut self, table: Arc<InlineCandidateTable>) {
        self.cands.clear();
        self.shared = Some(table);
    }

    pub fn warm_candidate(
        &mut self,
        ws: &Workspace,
        scx: &ScalarCtx,
        inst: Instance,
        concrete_plans: &ConcretePlans,
    ) -> Result<InlineCandidateWarm, CompilerError> {
        let candidate = classify(ws, scx, &inst, concrete_plans)?.map(Arc::new);
        Ok(InlineCandidateWarm {
            key: inst,
            candidate,
        })
    }

    pub fn invalidate(&mut self, fref: FuncRef) {
        self.cands.retain(|inst, _| inst.func != fref);
    }
}

fn annotation_gate(has_inline: bool, has_pure: bool) -> bool {
    has_inline || has_pure
}

#[derive(Default)]
struct LiveClosure {
    blocks: HashSet<Idx<Block>>,
    insts: HashSet<Idx<Inst>>,
    cells: HashSet<Idx<Cell>>,
}

/// Share these tables when compacting several regions from one body.
pub(crate) struct ArenaFacts {
    /// `(emit cell, emit block)` of every lambda parameter, sorted.
    lambda_params: Vec<(u32, u32)>,
    /// `(emit cell, emit block, block)` of every cell an instruction names,
    /// sorted and unique by the emit pair.
    uses: Vec<(u32, u32, Idx<Block>)>,
    /// `(block, cell)` of every `Load`.
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
    ($target:expr, $source:expr, $inst_map:expr, $($field:ident),+ $(,)?) => {
        $(
            for (old_id, value) in &$source.$field {
                if let Some(&new_id) = $inst_map.get(old_id) {
                    $target.$field.insert(new_id, value.clone());
                }
            }
        )+
    };
}

fn copy_inst_metadata(
    target: &mut MirFunc,
    source: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
) {
    copy_inst_tables!(
        target,
        source,
        inst_map,
        spec_bindings,
        callee_instances,
        call_abi_plans,
        call_rep_plans,
        flat_call_lanes,
        fn_value_symbols,
        code_value_identities,
        lambda_abi_fingerprints,
        lambda_abi_representations,
    );
    for (old_id, span) in source.inst_spans.iter() {
        if let Some(&new_id) = inst_map.get(&old_id) {
            target.inst_spans.insert(new_id, *span);
        }
    }
}

fn copy_projection_metadata(
    target: &mut MirFunc,
    source: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
) {
    target.synthetic_projections.extend(
        source
            .synthetic_projections
            .iter()
            .filter_map(|id| inst_map.get(id)),
    );
    copy_inst_tables!(target, source, inst_map, flat_box_widths);
}

fn compact_live_template(
    mut source: MirFunc,
    live: &LiveClosure,
) -> (MirFunc, HashMap<Idx<Block>, Idx<Block>>) {
    let nested = std::mem::take(&mut source.nested_fns);
    let facts = ArenaFacts::of(&source);
    compact_region(&source, &source, &facts, nested, live, &[])
}

fn compact_region(
    header: &MirFunc,
    arena: &MirFunc,
    facts: &ArenaFacts,
    nested: Vec<MirFunc>,
    live: &LiveClosure,
    extra_cleanup_loads: &[Idx<Cell>],
) -> (MirFunc, HashMap<Idx<Block>, Idx<Block>>) {
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

    let mut cell_map = HashMap::default();
    let mut cells = comfy_la_arena::Arena::with_capacity(live.cells.len());
    for &id in &live_cells {
        cell_map.insert(id, cells.alloc(arena.cells[id].clone()));
    }

    let mut block_map = HashMap::default();
    let mut blocks = comfy_la_arena::Arena::with_capacity(live.blocks.len());
    for &id in &live_blocks {
        block_map.insert(id, blocks.alloc(Block::new()));
    }

    let mut inst_map = HashMap::default();
    let mut insts = comfy_la_arena::Arena::with_capacity(live.insts.len());
    for &id in &live_insts {
        inst_map.insert(id, insts.alloc(arena.insts[id].clone()));
    }
    for (_, inst) in insts.iter_mut() {
        remap_clone(inst, &inst_map, &cell_map, &block_map);
    }
    for (&old_id, &new_id) in &block_map {
        blocks[new_id] = remap_block(&arena.blocks[old_id], &inst_map, &block_map);
    }

    let mut template = MirFunc::new(header.name);
    template.code_identity = header.code_identity.clone();
    template.code_abi_fingerprint = header.code_abi_fingerprint.clone();
    template.code_compatibility = header.code_compatibility.clone();
    template.nested_fn_identities = arena.nested_fn_identities.clone();
    template.rep_arena = arena.rep_arena.clone();
    template.params = header.params.iter().map(|param| cell_map[param]).collect();
    template.value_lets = header.value_lets;
    template.value_leaves = header.value_leaves;
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
            let mapped = cell_map.get(cell).copied()?;
            (!live.blocks.contains(block)).then_some(mapped)
        })
        .collect();
    cleanup_load_cells.extend(
        extra_cleanup_loads
            .iter()
            .filter_map(|cell| cell_map.get(cell).copied()),
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
    template.entry = block_map[&header.entry];
    template.is_coroutine = header.is_coroutine;
    template.resume_arg = header.resume_arg.map(|cell| cell_map[&cell]);
    template.coro_defers = header
        .coro_defers
        .iter()
        .filter(|&(_, block)| live.blocks.contains(block))
        .map(|(flag, block)| (cell_map[flag], block_map[block]))
        .collect();
    template.for_iter_headers = arena
        .for_iter_headers
        .iter()
        .filter(|header| live.blocks.contains(header))
        .map(|header| block_map[header])
        .collect();
    template.hoisted_fn_globals = arena.hoisted_fn_globals.clone();

    copy_inst_metadata(&mut template, arena, &inst_map);
    copy_projection_metadata(&mut template, arena, &inst_map);
    copy_inst_tables!(
        template,
        arena,
        inst_map,
        bound_trees,
        resolved_bound_trees,
        param_default_guards,
    );
    for (param, values) in &arena.param_default_values {
        let values = values
            .iter()
            .filter_map(|value| inst_map.get(value).copied())
            .collect::<Vec<_>>();
        if !values.is_empty() {
            template.param_default_values.insert(*param, values);
        }
    }
    for (old_id, plan) in &arena.closure_env_plans {
        let Some(&new_id) = inst_map.get(old_id) else {
            continue;
        };
        let captures = plan.captures().iter().map(|cell| cell_map[cell]).collect();
        let env_cell = plan.env_cell().map(|cell| cell_map[&cell]);
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
        &inst_map,
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
    let (mut compact, _) = compact_region(
        header,
        arena,
        facts,
        Vec::new(),
        &live,
        &arena.inline_cleanup_load_cells,
    );
    compact.param_default_materialized = arena.param_default_materialized.clone();
    compact
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
) -> Result<Option<CalleeBody>, CompilerError> {
    let fref = inst.func;
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern || f.inline_body.is_some() || f.is_coroutine {
        return Ok(None);
    }
    let has_inline = f
        .annotations
        .iter()
        .any(|a| a.name.name.as_str() == "inline");
    let has_pure = f.annotations.iter().any(|a| a.name.name.as_str() == "pure");
    // Bounded candidates can resolve their witnesses at the call site.
    let has_bounds = f.generic_bounds.iter().any(Option::is_some);
    if !annotation_gate(has_inline, has_pure) && !has_bounds {
        return Ok(None);
    }
    let Some(plan) = concrete_plans.get(inst) else {
        return Ok(None);
    };
    skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    let abi = skull_mir::lower::owner_abi_plan(ws, scx, concrete_plans, inst)?;
    let mf = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, concrete_plans)?;
    let canonical = Arc::new(mf);
    let mut mf = (*canonical).clone();
    // Apply the splice budget to the cleaned candidate.
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

    let mut rebound: HashSet<Idx<Cell>> = HashSet::default();
    for (_, inst) in mf.insts.iter() {
        inst.cell_writes_each(&mut |cell| {
            rebound.insert(cell);
        });
    }
    let coalescable_params: Vec<bool> = mf.params.iter().map(|p| !rebound.contains(p)).collect();
    let live = live_closure(&mf);
    let source_cells = mf.cells.len();
    let (mf, block_map) = compact_live_template(mf, &live);

    let mut reachable = Vec::new();
    let (kind, ret_arity) = match &mf.blocks[mf.entry].term {
        Term::Return(values) if !has_lambda => (Kind::Straight, values.len()),
        _ => {
            reachable.extend(source_reachable.iter().map(|block| block_map[block]));
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
    Ok(Some(CalleeBody {
        func: mf,
        canonical,
        subject: inst.clone(),
        abi,
        kind,
        ret_arity,
        reachable,
        insts: reachable_insts,
        source_cells,
        coalescable_params,
        forwardable_reads,
        forwards_reads: !has_lambda,
    }))
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
            forwarded.push((scratch.inst_map[&read], args[p]));
        }
    }
    if !forwarded.is_empty() {
        let to = |v: Idx<Inst>| forwarded_to(&forwarded, v);
        for &iid in &scratch.inst_ids {
            func.insts[iid].remap_uses(&to);
        }
        for source_block in &cand.reachable {
            func.blocks[scratch.block_map[source_block]]
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
            cache.cands.insert(inst.clone(), candidate);
            return Ok(cache.cands[inst].as_deref());
        }
        let candidate = classify(ws, scx, inst, concrete_plans)?.map(Arc::new);
        cache.cands.insert(inst.clone(), candidate);
    }
    Ok(cache.cands[inst].as_deref())
}

struct Site {
    pos: (Idx<Block>, usize),
    call_id: Idx<Inst>,
    args: Vec<Idx<Inst>>,
    dests: Option<Vec<Idx<Cell>>>,
    inst: Instance,
    abi: BackendFnAbiPlanHandle,
}

/// The delivery record must agree with the concrete owner already on the call.
fn call_target(
    func: &MirFunc,
    call_id: Idx<Inst>,
    fref: FuncRef,
    ws: &Workspace,
) -> Result<Option<(Instance, BackendFnAbiPlanHandle)>, CompilerError> {
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
    let inst = owner.clone();
    if let Some(delivery) = func.spec_bindings.get(&call_id) {
        if delivery != &inst {
            return Err(CompilerError::new_no_location(format!(
                "internal: delivery instance for `{}` disagrees with its concrete call owner",
                declaration.name,
            )));
        }
    }
    let abi = func.call_abi_plans.get(&call_id).cloned().ok_or_else(|| {
        CompilerError::new_no_location(format!(
            "internal: concrete call to `{}` has no lowered ABI plan",
            declaration.name,
        ))
    })?;
    Ok(Some((inst, abi)))
}

/// Resume at the last splice: it only inserts calls at or after that position.
/// Scan the root CFG, since a closure body's cells are still upvalues.
fn find_site(
    func: &MirFunc,
    ws: &Workspace,
    scx: &ScalarCtx,
    cache: &mut InlineCache,
    self_ref: FuncRef,
    top: &HashSet<Idx<Block>>,
    resume: Option<(Idx<Block>, usize)>,
    cell_budget: usize,
    pinned: &HashSet<Idx<Cell>>,
    concrete_plans: &ConcretePlans,
) -> Result<Option<Site>, CompilerError> {
    let mut started = resume.is_none();
    for (bid, _) in func.blocks.iter() {
        if !top.contains(&bid) {
            continue;
        }
        let start = match resume {
            Some((rb, ri)) if !started => {
                if bid != rb {
                    continue; // haven't reached the resume block yet
                }
                started = true;
                ri
            }
            _ => 0,
        };
        let insts = &func.blocks[bid].insts;
        for (idx, &iid) in insts.iter().enumerate().skip(start) {
            let (fref, args, dests) = match &func.insts[iid] {
                Inst::Call { func, args } => (*func, args, None),
                Inst::CallMulti { func, args, dests } => (*func, args, Some(dests)),
                _ => continue,
            };
            if fref == self_ref {
                continue;
            }
            let Some((inst, abi)) = call_target(func, iid, fref, ws)? else {
                continue;
            };
            let site = Site {
                pos: (bid, idx),
                call_id: iid,
                args: args.clone(),
                dests: dests.cloned(),
                inst,
                abi,
            };
            let candidate = get(cache, ws, scx, &site.inst, concrete_plans)?;
            let Some(cand) = candidate else {
                continue;
            };
            let mismatched = cand.subject != site.inst
                || cand.abi.as_ref() != site.abi.as_ref()
                || cand.func.params.len() != site.args.len()
                || site
                    .dests
                    .as_ref()
                    .map_or(cand.ret_arity > 1, |dests| cand.ret_arity != dests.len());
            // A coalesced argument already occupies a counted caller cell.
            let over_budget = site.inst.args.is_empty()
                && cell_budget + cand.budget_cells() + cand.func.params.len() > CELL_BUDGET
                && cell_budget + cand.budget_cells() + cand.func.params.len()
                    - site_coalesced_cells(func, cand, &site, pinned)
                    > CELL_BUDGET;
            if !mismatched && !over_budget {
                return Ok(Some(site));
            }
        }
    }
    Ok(None)
}

fn site_coalesced_cells(
    func: &MirFunc,
    cand: &CalleeBody,
    site: &Site,
    pinned: &HashSet<Idx<Cell>>,
) -> usize {
    for_each_coalesced(func, cand, &site.args, site.pos, pinned, |_, _| {})
}

// Allocate every clone before remapping its operands.
fn clone_body(caller: &mut MirFunc, callee: &MirFunc, scratch: &mut InlineScratch, kind: Kind) {
    caller.ensure_emit_ids();
    let (inst_base, cell_base, block_base) = caller.emit_nexts().unwrap();
    caller.insts.reserve(callee.insts.len());
    caller
        .cells
        .reserve(callee.cells.len().saturating_sub(scratch.overrides.len()));
    scratch.cell_map.clear();
    scratch.cell_map.reserve(callee.cells.len());
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
    scratch.block_map.clear();
    if kind == Kind::General {
        import_cleanup_load_cells(caller, callee, &scratch.cell_map);
        let forced = map_forced_cells(caller, callee, &scratch.overrides, cell_base, block_base);
        let ids = caller.emit_ids.as_mut().unwrap();
        for (cell, block) in forced {
            ids.forced_cells.push(cell);
            ids.forced_cell_blocks.push(block);
        }
        caller.blocks.reserve(callee.blocks.len());
        scratch.block_map.reserve(callee.blocks.len());
        for (bid, _) in callee.blocks.iter() {
            let nbid = caller.alloc_block_with_emit_id(block_base + callee.emit_block_id(bid));
            scratch.block_map.insert(bid, nbid);
        }
    }
    scratch.inst_map.clear();
    scratch.inst_map.reserve(callee.insts.len());
    scratch.inst_ids.clear();
    scratch.inst_ids.reserve(callee.insts.len());
    for (iid, inst) in callee.insts.iter() {
        let nid =
            caller.alloc_inst_with_emit_id(inst.clone(), inst_base + callee.emit_inst_id(iid));
        scratch.inst_map.insert(iid, nid);
        scratch.inst_ids.push(nid);
    }
    copy_projection_metadata(caller, callee, &scratch.inst_map);
    for &nid in &scratch.inst_ids {
        remap_clone(
            &mut caller.insts[nid],
            &scratch.inst_map,
            &scratch.cell_map,
            &scratch.block_map,
        );
    }
    if kind == Kind::General {
        for (bid, block) in callee.blocks.iter() {
            caller.blocks[scratch.block_map[&bid]] =
                remap_block(block, &scratch.inst_map, &scratch.block_map);
        }
        caller.for_iter_headers.extend(
            callee
                .for_iter_headers
                .iter()
                .map(|header| scratch.block_map[header]),
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
    cell_map: &HashMap<Idx<Cell>, Idx<Cell>>,
) {
    if callee.inline_cleanup_load_cells.is_empty() {
        return;
    }
    let mut existing: HashSet<Idx<Cell>> =
        caller.inline_cleanup_load_cells.iter().copied().collect();
    for &cell in &callee.inline_cleanup_load_cells {
        if let Some(&mapped) = cell_map.get(&cell) {
            if existing.insert(mapped) {
                caller.inline_cleanup_load_cells.push(mapped);
            }
        }
    }
}

fn remap_clone(
    inst: &mut Inst,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    cm: &HashMap<Idx<Cell>, Idx<Cell>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    inst.remap_uses(&|x| im[&x]);
    match inst {
        Inst::Load(c) => *c = cm[c],
        Inst::Store { cell, .. } => *cell = cm[cell],
        Inst::CallMulti { dests, .. }
        | Inst::CallBoundMulti { dests, .. }
        | Inst::CallValueMulti { dests, .. } => {
            for d in dests {
                *d = cm[d];
            }
        }
        Inst::MapNext {
            key_cell, val_cell, ..
        } => {
            *key_cell = cm[key_cell];
            *val_cell = cm[val_cell];
        }
        Inst::Lambda { params, entry } => {
            for p in params {
                *p = cm[p];
            }
            *entry = bm[entry];
        }
        _ => {}
    }
}

fn remap_block(
    block: &Block,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) -> Block {
    let mut term = block.term.clone();
    term.remap_uses(&|x| im[&x]);
    term.remap_targets(&|x| bm[&x]);
    Block {
        insts: block.insts.iter().map(|id| im[id]).collect(),
        term,
    }
}

/// Coalesce only if the argument cell still holds the value its load read.
fn coalesce_overrides(
    caller: &MirFunc,
    cand: &CalleeBody,
    args: &[Idx<Inst>],
    pos: (Idx<Block>, usize),
    pinned: &HashSet<Idx<Cell>>,
    scratch: &mut InlineScratch,
) -> usize {
    let callee = &cand.func;
    scratch.overrides.clear();
    scratch.coalesced.clear();
    scratch.coalesced.resize(callee.params.len(), false);
    for_each_coalesced(caller, cand, args, pos, pinned, |i, c| {
        scratch.overrides.insert(callee.params[i], c);
        scratch.coalesced[i] = true;
    })
}

fn for_each_coalesced(
    caller: &MirFunc,
    cand: &CalleeBody,
    args: &[Idx<Inst>],
    pos: (Idx<Block>, usize),
    pinned: &HashSet<Idx<Cell>>,
    mut visit: impl FnMut(usize, Idx<Cell>),
) -> usize {
    let mut preds = None;
    let mut count = 0;
    for (i, arg) in args.iter().enumerate().take(cand.func.params.len()) {
        if !cand.coalescable_params.get(i).copied().unwrap_or(false) {
            continue;
        }
        // Coalescing a converted load would erase its numeric conversion.
        let load = *arg;
        let Inst::Load(c) = caller.insts[load] else {
            continue;
        };
        if snapshot_holds(caller, pinned, &mut preds, c, load, pos) {
            visit(i, c);
            count += 1;
        }
    }
    count
}

/// A copied argument is a snapshot until the call and throughout the splice.
fn snapshot_holds(
    caller: &MirFunc,
    pinned: &HashSet<Idx<Cell>>,
    preds: &mut Option<HashMap<Idx<Block>, Vec<Idx<Block>>>>,
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
        let mut map: HashMap<Idx<Block>, Vec<Idx<Block>>> = HashMap::default();
        for (from, block) in caller.blocks.iter() {
            skull_mir::cfg::successors(&block.term, |to| map.entry(to).or_default().push(from));
        }
        map
    });
    let mut seen: HashSet<Idx<Block>> = HashSet::default();
    let mut stack: Vec<Idx<Block>> = preds.get(&bid).cloned().unwrap_or_default();
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
        stack.extend(preds.get(&b).into_iter().flatten().copied());
    }
    found
}

fn pin_cells(
    func: &MirFunc,
    top: &HashSet<Idx<Block>>,
    blocks_from: usize,
    pinned: &mut HashSet<Idx<Cell>>,
) {
    for (bid, block) in func.blocks.iter().skip(blocks_from) {
        if top.contains(&bid) {
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
            scx.interner(),
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
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
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
    for new_id in inst_map.values() {
        remap_nested_global(&mut func.insts[*new_id], &names);
    }
    let mut nested = callee.nested_fns.clone();
    rename_nested_tree(&mut nested, &names);
    func.nested_fns.extend(nested);
    names
}

fn import_splice_metadata(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    nested_names: &HashMap<Ustr, Ustr>,
) {
    copy_inst_metadata(func, callee, inst_map);
    copy_inst_tables!(
        func,
        callee,
        inst_map,
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
    import_call_sites(func, callee, inst_map, None);
}

fn import_call_sites(
    target: &mut MirFunc,
    source: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    missing_owner: Option<&str>,
) {
    // The site keeps its source expression arena when its owner changes tables.
    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, cs) in &source.call_sites {
        let Some(&new_id) = inst_map.get(old_id) else {
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
    uses: &mut UseIndex,
) -> Result<usize, CompilerError> {
    scratch.added_blocks.clear();
    let callee = &cand.func;
    let (bid, idx) = site.pos;
    let args = &site.args;
    let site_tree = func.bound_trees.remove(&site.call_id);
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id);
    let coalesced_cells = coalesce_overrides(func, cand, args, (bid, idx), pinned, scratch);
    clone_body(func, callee, scratch, cand.kind);
    let repack_mark = func.insts.len();
    resolve_cloned_bound_calls(
        func,
        callee,
        &scratch.inst_map,
        site_tree.as_ref(),
        site_resolved_tree.as_ref(),
        ws,
        scx,
        concrete_plans,
    )?;
    let names = import_nested_fns(func, callee, &scratch.inst_map);
    import_splice_metadata(func, callee, &scratch.inst_map, &names);
    let forwarded = forward_param_reads(func, cand, args, scratch);
    if cand.kind == Kind::General && !forwarded.is_empty() {
        for source in &cand.reachable {
            func.blocks[scratch.block_map[source]]
                .insts
                .retain(|inst| !forwarded.iter().any(|(read, _)| read == inst));
        }
    }
    for &iid in &scratch.inst_ids {
        uses.add_inst(func, iid);
    }
    uses.add_insts_from(func, repack_mark);
    for &bid in scratch.block_map.values() {
        uses.add_term(func, bid);
    }
    uses.remove_inst(func, site.call_id);
    match cand.kind {
        Kind::Straight => splice_straight(func, &site, cand, scratch, uses, &forwarded),
        Kind::General => splice_general(func, &site, cand, scratch, uses),
    }
    Ok(coalesced_cells)
}

fn bind_params(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &InlineScratch,
    uses: &mut UseIndex,
    prefix: &mut Vec<Idx<Inst>>,
) {
    let args = &site.args;
    for (i, (param, &arg)) in cand.func.params.iter().zip(args).enumerate() {
        if scratch.coalesced[i] {
            continue;
        }
        let store = func.alloc_inst(Inst::Store {
            cell: scratch.cell_map[param],
            value: arg,
        });
        if !copy_unread(cand, i) || !forwards_arg(func, arg) {
            uses.add_inst(func, store);
            prefix.push(store);
        }
    }
}

fn splice_straight(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &InlineScratch,
    uses: &mut UseIndex,
    forwarded: &[(Idx<Inst>, Idx<Inst>)],
) {
    let callee = &cand.func;
    let (bid, idx) = site.pos;
    let old = std::mem::take(&mut func.blocks[bid].insts);
    let mut new_insts = old[..idx].to_vec();
    bind_params(func, site, cand, scratch, uses, &mut new_insts);
    new_insts.extend(
        callee.blocks[callee.entry]
            .insts
            .iter()
            .map(|inst| scratch.inst_map[inst])
            .filter(|inst| !forwarded.iter().any(|(read, _)| read == inst)),
    );

    match &site.dests {
        Some(dests) => {
            let values = match &callee.blocks[callee.entry].term {
                Term::Return(values) => values,
                _ => unreachable!("straight candidate entry must end in Return"),
            };
            for (dest, rv) in dests.iter().zip(values.iter()) {
                let st = func.alloc_inst(Inst::Store {
                    cell: *dest,
                    value: forwarded_to(forwarded, scratch.inst_map[rv]),
                });
                uses.add_inst(func, st);
                new_insts.push(st);
            }
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
        }
        None => {
            let call_id = site.call_id;
            // Replace the call in all of its users, including successor blocks.
            let to = match &callee.blocks[callee.entry].term {
                Term::Return(values) => match values
                    .first()
                    .map(|value| forwarded_to(forwarded, scratch.inst_map[value]))
                {
                    Some(value) => value,
                    None => {
                        func.insts[call_id] = Inst::Const(Lit::Nil);
                        new_insts.push(call_id);
                        call_id
                    }
                },
                _ => unreachable!("straight candidate entry must end in Return"),
            };
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
            uses.replace(func, call_id, to);
        }
    }
}

fn splice_general(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &mut InlineScratch,
    uses: &mut UseIndex,
) {
    let (bid, idx) = site.pos;
    let callee = &cand.func;
    let entry_clone = scratch.block_map[&callee.entry];

    let old = std::mem::take(&mut func.blocks[bid].insts);
    let before: Vec<Idx<Inst>> = old[..idx].to_vec();
    let after: Vec<Idx<Inst>> = old[idx + 1..].to_vec();
    let orig_term = func.blocks[bid].term.clone();
    uses.remove_term(func, bid);

    let cont = func.new_block();
    func.blocks[cont].insts = after;
    func.blocks[cont].term = orig_term;
    uses.add_term(func, cont);

    // Reuse the call ID for a load from the result cell.
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

    let mut prefix = before;
    bind_params(func, site, cand, scratch, uses, &mut prefix);
    func.blocks[bid].insts = prefix;
    func.blocks[bid].term = Term::Goto(entry_clone);

    // Only the callee CFG returns to the caller. Lambda returns stay in their sub-CFGs.
    for &source_block in &cand.reachable {
        let nb = scratch.block_map[&source_block];
        let vs = match &func.blocks[nb].term {
            Term::Return(vs) => vs.clone(),
            _ => continue,
        };
        uses.remove_term(func, nb);
        match &site.dests {
            None => {
                let rc = result_cell.unwrap();
                let v = match vs.first() {
                    Some(v) => *v,
                    None => {
                        let nil = func.alloc_inst(Inst::Const(Lit::Nil));
                        func.blocks[nb].insts.push(nil);
                        uses.add_inst(func, nil);
                        nil
                    }
                };
                let st = func.alloc_inst(Inst::Store { cell: rc, value: v });
                func.blocks[nb].insts.push(st);
                uses.add_inst(func, st);
            }
            Some(dests) => {
                for (dest, rv) in dests.iter().zip(vs.iter()) {
                    let st = func.alloc_inst(Inst::Store {
                        cell: *dest,
                        value: *rv,
                    });
                    func.blocks[nb].insts.push(st);
                    uses.add_inst(func, st);
                }
            }
        }
        func.blocks[nb].term = Term::Goto(cont);
        uses.add_term(func, nb);
    }
    scratch
        .added_blocks
        .extend(cand.reachable.iter().map(|b| scratch.block_map[b]));
    scratch.added_blocks.push(cont);
}

pub(crate) fn inline_functions_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<(HashSet<FuncRef>, Vec<SpliceRecord>), CompilerError> {
    let mut spliced = HashSet::default();
    let mut records: Vec<SpliceRecord> = Vec::new();
    if inline_off_active() {
        return Ok((spliced, records));
    }
    let mut top: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    let mut pinned: HashSet<Idx<Cell>> = HashSet::default();
    pin_cells(func, &top, 0, &mut pinned);
    let mut uses: Option<UseIndex> = None;
    let mut resume: Option<(Idx<Block>, usize)> = None;
    let mut cell_budget = func.cells.len() + func.value_leaves;
    let mut budget = MAX_INLINES_PER_FN;
    while budget > 0 {
        let Some(site) = find_site(
            func,
            ws,
            scx,
            cache,
            self_ref,
            &top,
            resume,
            cell_budget,
            &pinned,
            concrete_plans,
        )?
        else {
            break;
        };
        let pos = site.pos;
        let fref = site.inst.func;
        let uses = uses.get_or_insert_with(|| UseIndex::build(func));
        let (cands, scratch) = (&cache.cands, &mut cache.scratch);
        let cand = cands[&site.inst]
            .as_ref()
            .expect("site implies a cached candidate");
        let result_cell = usize::from(cand.kind == Kind::General && site.dests.is_none());
        let blocks_before = func.blocks.len();
        let coalesced_cells = splice(
            func,
            site,
            cand,
            ws,
            scx,
            concrete_plans,
            &pinned,
            scratch,
            uses,
        )?;
        top.extend(scratch.added_blocks.drain(..));
        pin_cells(func, &top, blocks_before, &mut pinned);
        records.push(SpliceRecord {
            callee: fref,
            callee_insts: cand.insts,
        });
        // The caller must import cells referenced by the spliced body.
        func.statics.extend(cand.func.statics.iter().cloned());
        func.comptime_reads
            .extend(cand.func.comptime_reads.iter().cloned());
        func.struct_registry_roots
            .extend(cand.func.struct_registry_roots.iter().copied());
        func.value_lets += cand.func.value_lets;
        func.value_leaves += cand.func.value_leaves;
        cell_budget += cand.budget_cells().saturating_sub(coalesced_cells) + result_cell;
        spliced.insert(fref);
        resume = Some(pos);
        budget -= 1;
    }
    if func.statics.len() > 1 {
        let mut seen: HashSet<ustr::Ustr> = HashSet::default();
        func.statics.retain(|sc| seen.insert(sc.cell));
    }
    #[cfg(test)]
    if let Some(uses) = &uses {
        uses.record_drift(func);
    }
    Ok((spliced, records))
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

    /// `bound::repack_bound_next_element` allocates the element's
    /// leaf loads, its rebuilt aggregate and the store into the element cell
    /// AFTER the splice cloned the callee. The splice's `UseIndex` must cover
    /// those users like every other instruction it adds.
    ///
    /// Consistent plans never reach the repack on this path: the lowering sees
    /// the same element shape as the witness and splits the element itself.
    /// The witness result is therefore perturbed to `(bool, Pair)` under an
    /// `Iterator<int>` body, the same disagreement the standalone rewrite's
    /// plan test uses, so the production splice has to rebuild a `Pair`.
    #[test]
    fn use_index_covers_bound_element_repack_after_splice() {
        let src = r#"
nobox value Pair { x: int, y: int }
struct Counter { n: int }
fn next(c: Counter) -> (bool, int) {
    if c.n >= 3 { return (false, 0) }
    c.n = c.n + 1
    return (true, c.n)
}
fn sum_all<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for x in it { s = s + 1 }
    return s
}
fn top() -> int { return sum_all(Counter { n: 0 }) }
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
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| skull_mir::mangle::declared_name(f.name.as_str()) == "top")
            .expect("top");
        let top = FuncRef {
            module: mid,
            func: fid,
        };
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let graph =
            skull_graph::instance_graph::build_in(&ws, vec![top], std::sync::Arc::clone(&interner));
        let concrete = skull_graph::concrete_body::build(&ws, &graph);
        let scx = ScalarCtx::new(&ws).sharing(std::sync::Arc::clone(&interner));

        let witness = concrete
            .plans
            .values()
            .flat_map(|plan| plan.bound_tree.next.values())
            .find_map(|next| match next {
                skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. } => {
                    Some(instance.clone())
                }
                skull_mir::bound::ResolvedBoundNext::CallerGeneric(_) => None,
            })
            .expect("fixture has no concrete bound witness");
        // A context plans each callee ABI once. Lowering the bounded body from
        // the intact plans settles the witness ABI the lowering sizes the
        // element from, so only the repack's own signature read sees the
        // perturbation below.
        for plan in concrete.plans.values() {
            if !plan.bound_tree.is_empty() {
                skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
                    .expect("intact bounded body");
            }
        }
        let pair = skull_mir::monoty::MonoTy::App {
            ctor: skull_mir::monoty::MonoCtor::Nominal(
                *ws.struct_by_name
                    .keys()
                    .find(|name| skull_mir::mangle::declared_name(name.as_str()) == "Pair")
                    .expect("Pair definition"),
            ),
            params: Vec::new(),
        };
        let wrong_result = interner.intern(skull_mir::monoty::MonoTy::Tuple(vec![
            skull_mir::monoty::MonoTy::Bool,
            pair,
        ]));
        let mut plans = (*concrete.plans).clone();
        for (inst, plan) in plans.iter_mut() {
            let plan = std::sync::Arc::make_mut(plan);
            if *inst == witness {
                std::sync::Arc::make_mut(&mut plan.signature).result = Some(wrong_result);
            }
            for callee in plan.callees.values_mut() {
                if callee.instance == witness {
                    std::sync::Arc::make_mut(&mut callee.signature).result = Some(wrong_result);
                }
            }
        }

        let plan = plans.get(&Instance::base(top)).expect("top plan");
        let mut func = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &plans)
            .expect("lower top");
        let mut cache = InlineCache::default();
        inline_functions_with_plans(&ws, &scx, top, &mut func, &mut cache, &plans)
            .expect("inline top");

        let rebuilt = func.blocks.iter().any(|(_, block)| {
            block.insts.iter().any(|iid| {
                matches!(
                    &func.insts[*iid],
                    Inst::MakeValueStruct {
                        form: ValueCtorForm::Leaves,
                        ..
                    }
                )
            })
        });
        assert!(
            rebuilt,
            "the splice did not repack a splatted bound element"
        );
        let (missing, stale) = USE_INDEX_DRIFT.with(|drift| drift.borrow().clone());
        assert!(
            missing.is_empty() && stale.is_empty(),
            "inliner UseIndex drifted from the caller body\nmissing: {missing:#?}\nstale: {stale:#?}"
        );
    }
}
