//! Splice eligible direct calls and record the concrete bodies embedded in each caller.
//! Annotation gating limits the number of chunks a body edit invalidates.

use bone_core::prelude::Instant;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smallvec::SmallVec;
use std::cell::Cell as ThreadCell;
use std::collections::BTreeMap;
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

/// Process-wide switch for comparing emitted code with and without inlining.
fn inline_disabled_by_env() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| env_disables_inline(std::env::var("ROCK_SKULL_INLINE_OFF").ok().as_deref()))
}

fn env_disables_inline(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true"))
}

fn inline_disabled() -> bool {
    inline_off_active() || inline_disabled_by_env()
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
    bounded: bool,
    template: InlineCloneStats,
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
        self.template.source_cells + self.func.value_lets
    }
}

pub struct SpliceRecord {
    pub callee: FuncRef,
    pub straight: bool,
    pub bounded: bool,
    pub transitive: bool,
    pub callee_insts: usize,
    pub clone: InlineCloneStats,
    pub splice_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineSideTableFamily {
    pub source_rows: usize,
    pub examined: usize,
    pub copied: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineSideTableStats {
    pub spec_bindings: InlineSideTableFamily,
    pub callee_instances: InlineSideTableFamily,
    pub abi_plans: InlineSideTableFamily,
    pub representation_plans: InlineSideTableFamily,
    pub closure_environment_plans: InlineSideTableFamily,
    pub raw_bound_trees: InlineSideTableFamily,
    pub resolved_bound_trees: InlineSideTableFamily,
    pub function_value_symbols: InlineSideTableFamily,
    pub instruction_spans: InlineSideTableFamily,
    pub call_sites: InlineSideTableFamily,
    pub call_site_owners: InlineSideTableFamily,
    pub iterator_headers: InlineSideTableFamily,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineCloneStats {
    pub source_instructions: usize,
    pub source_cells: usize,
    pub source_blocks: usize,
    pub live_instructions: usize,
    pub live_cells: usize,
    pub live_blocks: usize,
    pub cloned_instructions: usize,
    pub cloned_cells: usize,
    pub cloned_blocks: usize,
    pub dead_instructions: usize,
    pub dead_cells: usize,
    pub dead_blocks: usize,
    pub dead_cloned_instructions: usize,
    pub dead_cloned_cells: usize,
    pub dead_cloned_blocks: usize,
    pub cleanup_load_roots: usize,
    pub inst_remap_entries: usize,
    pub cell_remap_entries: usize,
    pub block_remap_entries: usize,
    pub inst_remap_capacity: usize,
    pub cell_remap_capacity: usize,
    pub block_remap_capacity: usize,
    pub inst_remap_growth: usize,
    pub cell_remap_growth: usize,
    pub block_remap_growth: usize,
    pub inst_arena_reallocations: usize,
    pub cell_arena_reallocations: usize,
    pub block_arena_reallocations: usize,
    pub caller_reserved_instructions: usize,
    pub caller_reserved_cells: usize,
    pub caller_reserved_blocks: usize,
    pub argument_elements: usize,
    pub argument_stores: usize,
    pub destination_elements: usize,
    pub destination_stores: usize,
    pub block_prefix_copies: usize,
    pub block_tail_copies: usize,
    pub nested_function_imports: usize,
    pub static_imports: usize,
    pub side_tables: InlineSideTableStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineScanStats {
    pub enabled: bool,
    pub borrowed_block_scans: usize,
    pub instructions_scanned: usize,
    pub top_level_blocks_ns: u64,
    pub top_level_blocks_population: usize,
    pub call_instructions_examined: usize,
    pub call_target_resolutions: usize,
    pub straight_accepted: usize,
    pub general_accepted: usize,
    pub direct_splices: usize,
    pub transitive_splices: usize,
    pub resume_cursor_progress: usize,
    pub rescanned_instructions: usize,
    pub candidate_requests: usize,
    pub candidate_cache_hits: usize,
    pub candidate_derivations: usize,
    pub candidate_requested_keys: Vec<Instance>,
    pub candidate_lower_ns: u64,
    pub candidate_cleanup_ns: u64,
    pub candidate_eligibility_ns: u64,
    pub candidate_keys: Vec<Instance>,
    pub spliced_keys: Vec<Instance>,
    pub splice_ns: u64,
    pub ordinary_keys: Vec<Instance>,
    pub candidate_worker_derivations: BTreeMap<String, usize>,
    pub rejection_reasons: BTreeMap<String, usize>,
    pub clone: InlineCloneStats,
    pub use_index_build_ns: u64,
    pub use_index_values: usize,
    pub targeted_bindings: usize,
    pub targeted_binding_users: usize,
    pub targeted_binding_terminators: usize,
    pub targeted_binding_replacements: usize,
    pub targeted_binding_ns: u64,
    pub load_store: crate::LoadStoreCensus,
}

impl InlineCloneStats {
    pub fn add(&mut self, other: Self) {
        self.source_instructions += other.source_instructions;
        self.source_cells += other.source_cells;
        self.source_blocks += other.source_blocks;
        self.live_instructions += other.live_instructions;
        self.live_cells += other.live_cells;
        self.live_blocks += other.live_blocks;
        self.cloned_instructions += other.cloned_instructions;
        self.cloned_cells += other.cloned_cells;
        self.cloned_blocks += other.cloned_blocks;
        self.dead_instructions += other.dead_instructions;
        self.dead_cells += other.dead_cells;
        self.dead_blocks += other.dead_blocks;
        self.dead_cloned_instructions += other.dead_cloned_instructions;
        self.dead_cloned_cells += other.dead_cloned_cells;
        self.dead_cloned_blocks += other.dead_cloned_blocks;
        self.cleanup_load_roots += other.cleanup_load_roots;
        self.inst_remap_entries += other.inst_remap_entries;
        self.cell_remap_entries += other.cell_remap_entries;
        self.block_remap_entries += other.block_remap_entries;
        self.inst_remap_capacity += other.inst_remap_capacity;
        self.cell_remap_capacity += other.cell_remap_capacity;
        self.block_remap_capacity += other.block_remap_capacity;
        self.inst_remap_growth += other.inst_remap_growth;
        self.cell_remap_growth += other.cell_remap_growth;
        self.block_remap_growth += other.block_remap_growth;
        self.inst_arena_reallocations += other.inst_arena_reallocations;
        self.cell_arena_reallocations += other.cell_arena_reallocations;
        self.block_arena_reallocations += other.block_arena_reallocations;
        self.caller_reserved_instructions += other.caller_reserved_instructions;
        self.caller_reserved_cells += other.caller_reserved_cells;
        self.caller_reserved_blocks += other.caller_reserved_blocks;
        self.argument_elements += other.argument_elements;
        self.argument_stores += other.argument_stores;
        self.destination_elements += other.destination_elements;
        self.destination_stores += other.destination_stores;
        self.block_prefix_copies += other.block_prefix_copies;
        self.block_tail_copies += other.block_tail_copies;
        self.nested_function_imports += other.nested_function_imports;
        self.static_imports += other.static_imports;
        self.side_tables.add(other.side_tables);
    }
}

impl InlineSideTableStats {
    pub fn add(&mut self, other: Self) {
        macro_rules! add_family {
            ($field:ident) => {
                self.$field.source_rows += other.$field.source_rows;
                self.$field.examined += other.$field.examined;
                self.$field.copied += other.$field.copied;
            };
        }
        add_family!(spec_bindings);
        add_family!(callee_instances);
        add_family!(abi_plans);
        add_family!(representation_plans);
        add_family!(closure_environment_plans);
        add_family!(raw_bound_trees);
        add_family!(resolved_bound_trees);
        add_family!(function_value_symbols);
        add_family!(instruction_spans);
        add_family!(call_sites);
        add_family!(call_site_owners);
        add_family!(iterator_headers);
    }
}

impl InlineScanStats {
    pub fn add(&mut self, other: Self) {
        self.enabled |= other.enabled;
        self.borrowed_block_scans += other.borrowed_block_scans;
        self.instructions_scanned += other.instructions_scanned;
        self.top_level_blocks_ns += other.top_level_blocks_ns;
        self.top_level_blocks_population += other.top_level_blocks_population;
        self.call_instructions_examined += other.call_instructions_examined;
        self.call_target_resolutions += other.call_target_resolutions;
        self.straight_accepted += other.straight_accepted;
        self.general_accepted += other.general_accepted;
        self.direct_splices += other.direct_splices;
        self.transitive_splices += other.transitive_splices;
        self.resume_cursor_progress += other.resume_cursor_progress;
        self.rescanned_instructions += other.rescanned_instructions;
        self.candidate_requests += other.candidate_requests;
        self.candidate_cache_hits += other.candidate_cache_hits;
        self.candidate_derivations += other.candidate_derivations;
        self.candidate_requested_keys
            .extend(other.candidate_requested_keys);
        self.candidate_lower_ns += other.candidate_lower_ns;
        self.candidate_cleanup_ns += other.candidate_cleanup_ns;
        self.candidate_eligibility_ns += other.candidate_eligibility_ns;
        self.candidate_keys.extend(other.candidate_keys);
        self.spliced_keys.extend(other.spliced_keys);
        self.splice_ns += other.splice_ns;
        self.ordinary_keys.extend(other.ordinary_keys);
        for (worker, count) in other.candidate_worker_derivations {
            *self.candidate_worker_derivations.entry(worker).or_default() += count;
        }
        for (reason, count) in other.rejection_reasons {
            *self.rejection_reasons.entry(reason).or_default() += count;
        }
        self.clone.add(other.clone);
        self.use_index_build_ns += other.use_index_build_ns;
        self.use_index_values += other.use_index_values;
        self.targeted_bindings += other.targeted_bindings;
        self.targeted_binding_users += other.targeted_binding_users;
        self.targeted_binding_terminators += other.targeted_binding_terminators;
        self.targeted_binding_replacements += other.targeted_binding_replacements;
        self.targeted_binding_ns += other.targeted_binding_ns;
        self.load_store.add(&other.load_store);
    }
}

/// Cache one result per concrete instantiation, including rejected candidates.
type CacheKey = Instance;

pub struct InlineCandidateTable {
    entries: HashMap<CacheKey, Option<Arc<CalleeBody>>>,
    removed: HashSet<CacheKey>,
    parent: Option<Arc<InlineCandidateTable>>,
    stats: InlineCandidateTableStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineCandidateProfile {
    pub lower_ns: u64,
    pub full_clone_ns: u64,
    pub cleanup_ns: u64,
    pub compact_ns: u64,
    pub eligibility_ns: u64,
    pub lower_attempted: bool,
    pub lowered: bool,
    pub eligible: bool,
    pub rejected_before_lower: bool,
    pub raw_instructions: usize,
    pub raw_cells: usize,
    pub raw_blocks: usize,
    pub cleaned_instructions: usize,
    pub cleaned_cells: usize,
    pub cleaned_blocks: usize,
    pub full_clone_inst_capacity: usize,
    pub full_clone_cell_capacity: usize,
    pub full_clone_block_capacity: usize,
    pub compact_inst_capacity: usize,
    pub compact_cell_capacity: usize,
    pub compact_block_capacity: usize,
    pub worker: String,
    pub rejection_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineCandidateTableStats {
    pub keys: usize,
    pub warmed: usize,
    pub reused: usize,
    pub eligible: usize,
    pub rejected: usize,
    pub lower_ns: u64,
    pub cleanup_ns: u64,
    pub eligibility_ns: u64,
    pub profiles: BTreeMap<Instance, InlineCandidateProfile>,
    pub lower_ns_by_key: BTreeMap<Instance, u64>,
    pub worker_derivations: BTreeMap<String, usize>,
    pub rejection_reasons: BTreeMap<String, usize>,
}

pub struct InlineCandidateWarm {
    key: CacheKey,
    candidate: Option<Arc<CalleeBody>>,
    profile: InlineCandidateProfile,
}

fn adjust_count(map: &mut BTreeMap<String, usize>, key: &str, add: bool) {
    if add {
        *map.entry(key.to_string()).or_default() += 1;
    } else if let Some(count) = map.get_mut(key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(key);
        }
    }
}

fn adjust_candidate_profile(
    stats: &mut InlineCandidateTableStats,
    key: &Instance,
    profile: &InlineCandidateProfile,
    add: bool,
) {
    macro_rules! adjust {
        ($field:ident) => {
            if add {
                stats.$field += profile.$field;
            } else {
                stats.$field = stats.$field.saturating_sub(profile.$field);
            }
        };
    }
    adjust!(lower_ns);
    adjust!(cleanup_ns);
    adjust!(eligibility_ns);
    if add {
        stats.eligible += usize::from(profile.eligible);
        stats.rejected += usize::from(!profile.eligible);
    } else {
        stats.eligible = stats.eligible.saturating_sub(usize::from(profile.eligible));
        stats.rejected = stats
            .rejected
            .saturating_sub(usize::from(!profile.eligible));
    }
    if !profile.worker.is_empty() {
        adjust_count(&mut stats.worker_derivations, &profile.worker, add);
    }
    if let Some(reason) = profile.rejection_reason {
        adjust_count(&mut stats.rejection_reasons, reason, add);
    }
    if profile.lowered {
        if add {
            stats.lower_ns_by_key.insert(key.clone(), profile.lower_ns);
        } else {
            stats.lower_ns_by_key.remove(key);
        }
    }
}

impl InlineCandidateTable {
    pub fn from_warms_with_base(
        base: Option<&Self>,
        mut keys: Vec<CacheKey>,
        mut warms: Vec<InlineCandidateWarm>,
    ) -> Self {
        keys.sort();
        keys.dedup();
        warms.sort_by(|a, b| a.key.cmp(&b.key));
        let mut warmed = HashMap::default();
        warmed.reserve(warms.len());
        let mut stats = InlineCandidateTableStats {
            keys: keys.len(),
            warmed: warms.len(),
            ..InlineCandidateTableStats::default()
        };
        let census_enabled = warms.iter().any(|warm| !warm.profile.worker.is_empty())
            || base.is_some_and(|table| !table.stats.profiles.is_empty());
        if census_enabled {
            if let Some(base) = base {
                stats.profiles = base.stats.profiles.clone();
            }
        }
        for warm in warms {
            adjust_candidate_profile(&mut stats, &warm.key, &warm.profile, true);
            if census_enabled {
                stats.profiles.insert(warm.key.clone(), warm.profile);
            }
            let previous = warmed.insert(warm.key, warm.candidate);
            assert!(
                previous.is_none(),
                "inline candidate table received a duplicate key"
            );
        }
        stats.lower_ns_by_key = stats
            .profiles
            .iter()
            .filter_map(|(key, profile)| profile.lowered.then_some((key.clone(), profile.lower_ns)))
            .collect();
        stats.reused = keys.len().saturating_sub(stats.warmed);

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
            stats,
        }
    }

    /// Share unchanged candidates with the previous incremental snapshot.
    pub fn from_warms_incremental(
        base: Arc<Self>,
        warms: Vec<InlineCandidateWarm>,
        added: &[Instance],
        removed: &[Instance],
    ) -> Self {
        let mut entries = HashMap::default();
        let mut stats = base.stats.clone();
        let census_enabled =
            !stats.profiles.is_empty() || warms.iter().any(|warm| !warm.profile.worker.is_empty());
        if census_enabled {
            for key in removed {
                if let Some(profile) = stats.profiles.remove(key) {
                    adjust_candidate_profile(&mut stats, key, &profile, false);
                }
            }
        }
        for warm in &warms {
            if census_enabled {
                if let Some(profile) = stats.profiles.remove(&warm.key) {
                    adjust_candidate_profile(&mut stats, &warm.key, &profile, false);
                }
                adjust_candidate_profile(&mut stats, &warm.key, &warm.profile, true);
                stats
                    .profiles
                    .insert(warm.key.clone(), warm.profile.clone());
            }
        }
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
        stats.keys = stats
            .keys
            .saturating_add(added.len())
            .saturating_sub(removed.len());
        stats.warmed = entries.len();
        stats.reused = stats.keys.saturating_sub(stats.warmed);
        Self {
            entries,
            removed: removed_keys,
            parent: Some(base),
            stats,
        }
    }

    pub fn stats(&self) -> &InlineCandidateTableStats {
        &self.stats
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

#[derive(Clone)]
pub struct InlineCache {
    cands: HashMap<CacheKey, Option<Arc<CalleeBody>>>,
    shared: Option<Arc<InlineCandidateTable>>,
    scratch: InlineScratch,
    census_enabled: bool,
    census: InlineScanStats,
    worker: String,
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

    fn replace(
        &mut self,
        func: &mut MirFunc,
        from: Idx<Inst>,
        to: Idx<Inst>,
    ) -> (usize, usize, usize) {
        if from == to {
            return (0, 0, 0);
        }
        let sites = std::mem::take(self.slot(from));
        let mut instructions = 0;
        let mut terminators = 0;
        let replacements = ThreadCell::new(0usize);
        for site in sites {
            match site {
                UseSite::Inst(iid) => {
                    self.remove_inst(func, iid);
                    func.insts[iid].remap_uses(&|value| {
                        if value == from {
                            replacements.set(replacements.get() + 1);
                            to
                        } else {
                            value
                        }
                    });
                    self.add_inst(func, iid);
                    instructions += 1;
                }
                UseSite::Term(bid) => {
                    self.remove_term(func, bid);
                    func.blocks[bid].term.remap_uses(&|value| {
                        if value == from {
                            replacements.set(replacements.get() + 1);
                            to
                        } else {
                            value
                        }
                    });
                    self.add_term(func, bid);
                    terminators += 1;
                }
            }
        }
        (instructions, terminators, replacements.get())
    }
}

impl Default for InlineCache {
    fn default() -> Self {
        let census_enabled = std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug");
        let worker = if census_enabled {
            format!("{:?}", std::thread::current().id())
        } else {
            Default::default()
        };
        Self {
            cands: HashMap::default(),
            shared: None,
            scratch: InlineScratch::default(),
            census_enabled,
            census: InlineScanStats {
                enabled: census_enabled,
                ..InlineScanStats::default()
            },
            worker,
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

    pub fn census_enabled(&self) -> bool {
        self.census_enabled
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
        let started = self.census_enabled.then(Instant::now);
        let mut timing = InlineCandidateProfile::default();
        let result = classify(
            ws,
            scx,
            &inst,
            concrete_plans,
            self.census_enabled.then_some(&mut timing),
        );
        let eligibility_ns = started
            .map(|started| {
                (started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
                    .saturating_sub(timing.lower_ns)
                    .saturating_sub(timing.cleanup_ns)
                    .saturating_sub(timing.full_clone_ns)
                    .saturating_sub(timing.compact_ns)
            })
            .unwrap_or(0);
        let candidate = result?.map(Arc::new);
        timing.eligibility_ns = eligibility_ns;
        timing.eligible = timing.rejection_reason.is_none();
        timing.rejected_before_lower = !timing.eligible && !timing.lower_attempted;
        timing.worker = self.worker.clone();
        Ok(InlineCandidateWarm {
            key: inst,
            candidate,
            profile: timing,
        })
    }

    /// Drop every concrete specialization when the declaration's cached body is stale.
    pub fn invalidate(&mut self, fref: FuncRef) {
        self.cands.retain(|inst, _| inst.func != fref);
    }

    fn reset_census(&mut self) {
        self.census = InlineScanStats {
            enabled: self.census_enabled,
            ..InlineScanStats::default()
        };
    }

    fn rejection(&mut self, reason: &'static str) {
        if self.census_enabled {
            *self
                .census
                .rejection_reasons
                .entry(reason.to_string())
                .or_default() += 1;
        }
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

fn side_family(source_rows: usize, retained: usize) -> InlineSideTableFamily {
    InlineSideTableFamily {
        source_rows,
        examined: retained,
        copied: retained,
    }
}

fn template_counts(func: &MirFunc, live: &LiveClosure) -> InlineCloneStats {
    let live_instructions = live.insts.len();
    let live_cells = live.cells.len();
    let live_blocks = live.blocks.len();
    let live_inst = |id: &Idx<Inst>| live.insts.contains(id);
    let live_block = |id: &Idx<Block>| live.blocks.contains(id);
    let mut side = InlineSideTableStats {
        spec_bindings: side_family(
            func.spec_bindings.len(),
            func.spec_bindings.keys().filter(|id| live_inst(id)).count(),
        ),
        callee_instances: side_family(
            func.callee_instances.len(),
            func.callee_instances
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        abi_plans: side_family(
            func.call_abi_plans.len(),
            func.call_abi_plans
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        representation_plans: side_family(
            func.call_rep_plans.len(),
            func.call_rep_plans
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        closure_environment_plans: side_family(
            func.closure_env_plans.len(),
            func.closure_env_plans
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        raw_bound_trees: side_family(
            func.bound_trees.len(),
            func.bound_trees.keys().filter(|id| live_inst(id)).count(),
        ),
        resolved_bound_trees: side_family(
            func.resolved_bound_trees.len(),
            func.resolved_bound_trees
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        function_value_symbols: side_family(
            func.fn_value_symbols.len(),
            func.fn_value_symbols
                .keys()
                .filter(|id| live_inst(id))
                .count(),
        ),
        instruction_spans: side_family(
            func.inst_spans.iter().count(),
            func.inst_spans
                .iter()
                .filter(|(id, _)| live_inst(id))
                .count(),
        ),
        ..InlineSideTableStats::default()
    };
    let retained_call_sites = func.call_sites.keys().filter(|id| live_inst(id)).count();
    side.call_sites = side_family(func.call_sites.len(), retained_call_sites);
    let retained_owners: HashSet<u32> = func
        .call_sites
        .iter()
        .filter(|(id, _)| live_inst(id))
        .map(|(_, site)| site.owner.0)
        .collect();
    side.call_site_owners = side_family(func.site_owners.len(), retained_owners.len());
    side.iterator_headers = side_family(
        func.for_iter_headers.len(),
        func.for_iter_headers
            .iter()
            .filter(|id| live_block(id))
            .count(),
    );
    InlineCloneStats {
        source_instructions: func.insts.len(),
        source_cells: func.cells.len(),
        source_blocks: func.blocks.len(),
        live_instructions,
        live_cells,
        live_blocks,
        dead_instructions: func.insts.len().saturating_sub(live_instructions),
        dead_cells: func.cells.len().saturating_sub(live_cells),
        dead_blocks: func.blocks.len().saturating_sub(live_blocks),
        side_tables: side,
        ..InlineCloneStats::default()
    }
}

/// Compact the candidate once before it is used at multiple call sites.
fn compact_live_template(
    mut source: MirFunc,
    live: &LiveClosure,
) -> (MirFunc, HashMap<Idx<Block>, Idx<Block>>) {
    let nested = std::mem::take(&mut source.nested_fns);
    let facts = ArenaFacts::of(&source);
    compact_region(&source, &source, &facts, nested, live, &[])
}

/// Use the split body header with the original arena and its side tables.
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
    for id in &live.insts {
        let new_id = inst_map[id];
        remap_clone(&mut insts[new_id], &inst_map, &cell_map, &block_map);
    }
    for id in live.blocks.iter().copied() {
        let new_id = block_map[&id];
        let old = &arena.blocks[id];
        blocks[new_id].insts = old.insts.iter().map(|inst| inst_map[inst]).collect();
        let mut term = old.term.clone();
        remap_term_clone(&mut term, &inst_map, &block_map);
        blocks[new_id].term = term;
    }

    let mut emit_insts = vec![0; insts.len()];
    for (old, new) in &inst_map {
        emit_insts[crate::visit::raw(*new)] = arena.emit_inst_id(*old);
    }
    let mut emit_cells = vec![0; cells.len()];
    for (old, new) in &cell_map {
        emit_cells[crate::visit::raw(*new)] = arena.emit_cell_id(*old);
    }
    let mut emit_blocks = vec![0; blocks.len()];
    for (old, new) in &block_map {
        emit_blocks[crate::visit::raw(*new)] = arena.emit_block_id(*old);
    }

    let mut template = MirFunc::new(header.name);
    template.code_identity = header.code_identity.clone();
    template.code_abi_fingerprint = header.code_abi_fingerprint.clone();
    template.code_compatibility = header.code_compatibility.clone();
    template.lambda_abi_fingerprints = arena
        .lambda_abi_fingerprints
        .iter()
        .filter_map(|(old_id, fingerprint)| {
            inst_map
                .get(old_id)
                .map(|new_id| (*new_id, fingerprint.clone()))
        })
        .collect();
    template.lambda_abi_representations = arena
        .lambda_abi_representations
        .iter()
        .filter_map(|(old_id, representation)| {
            inst_map
                .get(old_id)
                .map(|new_id| (*new_id, representation.clone()))
        })
        .collect();
    template.nested_fn_identities = arena.nested_fn_identities.clone();
    template.rep_arena = arena.rep_arena.clone();
    template.params = header.params.iter().map(|param| cell_map[param]).collect();
    template.value_lets = header.value_lets;
    template.value_leaves = header.value_leaves;
    template.lower_timing_enabled = arena.lower_timing_enabled;
    template.lower_attribution = arena.lower_attribution.clone();
    template.abi_plan = header.abi_plan.clone();
    template.abi_representation = header.abi_representation.clone();
    template.global_plan = arena.global_plan.clone();
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
    template.emit_ids = Some(skull_mir::mir::EmitIds {
        insts: emit_insts,
        cells: emit_cells,
        blocks: emit_blocks,
        forced_cells: source_forced_cells.iter().map(|(cell, _)| *cell).collect(),
        forced_cell_blocks: source_forced_cells
            .iter()
            .map(|(_, block)| *block)
            .collect(),
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

    macro_rules! remap_inst_table {
        ($field:ident) => {
            for (old_id, value) in &arena.$field {
                if let Some(&new_id) = inst_map.get(old_id) {
                    template.$field.insert(new_id, value.clone());
                }
            }
        };
    }
    remap_inst_table!(bound_trees);
    remap_inst_table!(resolved_bound_trees);
    remap_inst_table!(spec_bindings);
    remap_inst_table!(callee_instances);
    remap_inst_table!(call_abi_plans);
    remap_inst_table!(call_rep_plans);
    remap_inst_table!(flat_call_lanes);
    remap_inst_table!(fn_value_symbols);
    remap_inst_table!(param_default_guards);
    remap_inst_table!(inst_logical_types);
    for (param, values) in &arena.param_default_values {
        let values = values
            .iter()
            .filter_map(|value| inst_map.get(value).copied())
            .collect::<Vec<_>>();
        if !values.is_empty() {
            template.param_default_values.insert(*param, values);
        }
    }
    for (old_id, identity) in &arena.code_value_identities {
        if let Some(&new_id) = inst_map.get(old_id) {
            template
                .code_value_identities
                .insert(new_id, identity.clone());
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
    for old_id in &arena.bound_next_split {
        if let Some(&new_id) = inst_map.get(old_id) {
            template.bound_next_split.insert(new_id);
        }
    }
    for (old_id, span) in arena.inst_spans.iter() {
        if let Some(&new_id) = inst_map.get(&old_id) {
            template.inst_spans.insert(new_id, *span);
        }
    }

    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, site) in &arena.call_sites {
        let Some(&new_id) = inst_map.get(old_id) else {
            continue;
        };
        let owner = match owner_map.get(&site.owner.0) {
            Some(&owner) => owner,
            None => {
                let owner = arena
                    .site_owners
                    .get(site.owner.0 as usize)
                    .expect("call-site owner must exist in the candidate template")
                    .clone();
                let owner = skull_mir::site::intern_owner(&mut template.site_owners, owner);
                owner_map.insert(site.owner.0, owner);
                owner
            }
        };
        template.call_sites.insert(
            new_id,
            skull_mir::site::CallSite {
                owner,
                site: site.site.clone(),
            },
        );
    }

    template.nested_fns = nested
        .into_iter()
        .map(|nested| {
            let nested_live = live_closure(&nested);
            compact_live_template(nested, &nested_live).0
        })
        .collect();
    (template, block_map)
}

/// Copy only the hoisted body's region and preserve emission IDs.
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

#[derive(Clone, Copy, Default)]
struct CloneMapStats {
    inst_entries: usize,
    cell_entries: usize,
    block_entries: usize,
    inst_capacity: usize,
    cell_capacity: usize,
    block_capacity: usize,
    inst_growth: usize,
    cell_growth: usize,
    block_growth: usize,
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

fn full_clone_stats(
    cand: &CalleeBody,
    straight: bool,
    argument_elements: usize,
    destination_elements: usize,
    prefix: usize,
    tail: usize,
    maps: CloneMapStats,
    coalesced_cells: usize,
    arena_reallocations: (usize, usize, usize),
) -> InlineCloneStats {
    let mut stats = cand.template;
    stats.cloned_instructions = stats.live_instructions;
    stats.cloned_cells = stats.live_cells;
    stats.cloned_blocks = if straight { 0 } else { stats.live_blocks };
    stats.dead_instructions = stats
        .source_instructions
        .saturating_sub(stats.live_instructions);
    stats.dead_cells = stats.source_cells.saturating_sub(stats.live_cells);
    stats.dead_blocks = stats.source_blocks.saturating_sub(stats.live_blocks);
    stats.dead_cloned_instructions = 0;
    stats.dead_cloned_cells = 0;
    stats.dead_cloned_blocks = 0;
    stats.cleanup_load_roots = if straight {
        0
    } else {
        cand.func.inline_cleanup_load_cells.len()
    };
    stats.inst_remap_entries = maps.inst_entries;
    stats.cell_remap_entries = maps.cell_entries;
    stats.block_remap_entries = maps.block_entries;
    stats.inst_remap_capacity = maps.inst_capacity;
    stats.cell_remap_capacity = maps.cell_capacity;
    stats.block_remap_capacity = maps.block_capacity;
    stats.inst_remap_growth = maps.inst_growth;
    stats.cell_remap_growth = maps.cell_growth;
    stats.block_remap_growth = maps.block_growth;
    stats.inst_arena_reallocations = arena_reallocations.0;
    stats.cell_arena_reallocations = arena_reallocations.1;
    stats.block_arena_reallocations = arena_reallocations.2;
    stats.caller_reserved_instructions = cand.func.insts.len();
    stats.caller_reserved_cells = cand.func.cells.len().saturating_sub(coalesced_cells);
    stats.caller_reserved_blocks = if straight { 0 } else { cand.func.blocks.len() };
    stats.argument_elements = argument_elements;
    stats.cloned_cells = stats.live_cells.saturating_sub(coalesced_cells);
    stats.argument_stores = cand
        .func
        .params
        .len()
        .min(argument_elements)
        .saturating_sub(coalesced_cells);
    stats.destination_elements = destination_elements;
    stats.destination_stores = destination_elements;
    stats.block_prefix_copies = prefix;
    stats.block_tail_copies = tail;
    stats.nested_function_imports = cand.func.nested_fns.len();
    stats.static_imports = cand.func.statics.len();
    stats
}

fn reject_candidate(
    timing: &mut Option<&mut InlineCandidateProfile>,
    reason: &'static str,
) -> Result<Option<CalleeBody>, CompilerError> {
    if let Some(timing) = timing.as_deref_mut() {
        timing.rejection_reason = Some(reason);
    }
    Ok(None)
}

fn classify(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
    mut timing: Option<&mut InlineCandidateProfile>,
) -> Result<Option<CalleeBody>, CompilerError> {
    let fref = inst.func;
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern {
        return reject_candidate(&mut timing, "extern");
    }
    if f.inline_body.is_some() {
        return reject_candidate(&mut timing, "inline_lua");
    }
    if f.is_coroutine {
        return reject_candidate(&mut timing, "coroutine");
    }
    let has_inline = f
        .annotations
        .iter()
        .any(|a| a.name.name.as_str() == "inline");
    let has_pure = f.annotations.iter().any(|a| a.name.name.as_str() == "pure");
    // Bounded candidates can resolve their witnesses at the call site.
    let has_bounds = f.generic_bounds.iter().any(Option::is_some);
    if !annotation_gate(has_inline, has_pure) && !has_bounds {
        return reject_candidate(&mut timing, "missing_inline_annotation");
    }
    let Some(plan) = concrete_plans.get(inst) else {
        return reject_candidate(&mut timing, "missing_concrete_plan");
    };
    skull_mir::lower::validate_concrete_bound_plan(f, plan)?;
    let abi = skull_mir::lower::owner_abi_plan(ws, scx, concrete_plans, inst)?;
    let lower_started = timing.as_ref().map(|_| Instant::now());
    if let Some(timing) = timing.as_deref_mut() {
        timing.lower_attempted = true;
    }
    let mf = skull_mir::lower::lower_instance_with_plans(ws, scx, plan, concrete_plans)?;
    if let Some(start) = lower_started {
        if let Some(timing) = timing.as_deref_mut() {
            timing.lower_ns = start.elapsed().as_nanos() as u64;
            timing.lowered = true;
            timing.raw_instructions = mf.insts.len();
            timing.raw_cells = mf.cells.len();
            timing.raw_blocks = mf.blocks.len();
        }
    }
    let canonical = Arc::new(mf);
    let clone_started = timing.as_ref().map(|_| Instant::now());
    let mut mf = (*canonical).clone();
    if let Some(start) = clone_started {
        std::hint::black_box(&mf);
        if let Some(timing) = timing.as_deref_mut() {
            timing.full_clone_ns = start.elapsed().as_nanos() as u64;
            timing.full_clone_inst_capacity = mf.insts.capacity();
            timing.full_clone_cell_capacity = mf.cells.capacity();
            timing.full_clone_block_capacity = mf.blocks.capacity();
        }
    }
    // Apply the splice budget to the cleaned candidate.
    let cleanup_started = timing.as_ref().map(|_| Instant::now());
    crate::cleanup(&mut mf)?;
    if let Some(start) = cleanup_started {
        if let Some(timing) = timing.as_deref_mut() {
            timing.cleanup_ns = start.elapsed().as_nanos() as u64;
            timing.cleaned_instructions = mf.insts.len();
            timing.cleaned_cells = mf.cells.len();
            timing.cleaned_blocks = mf.blocks.len();
        }
    }
    let mut has_lambda = false;
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Lambda { .. } => {
                // A bounded body can inline a nested lambda only through the CFG splice.
                if !has_bounds {
                    return reject_candidate(&mut timing, "lambda");
                }
                has_lambda = true;
            }
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } if *func == fref => {
                return reject_candidate(&mut timing, "self_recursive");
            }
            _ => {}
        }
    }

    let source_reachable = top_level_blocks(&mf);
    // An ordinary caller cannot supply the callee's coroutine state.
    for &b in &source_reachable {
        match &mf.blocks[b].term {
            Term::Yield { .. } => return reject_candidate(&mut timing, "yield_terminator"),
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
    let template = template_counts(&mf, &live);
    let compact_started = timing.as_ref().map(|_| Instant::now());
    let (mf, block_map) = compact_live_template(mf, &live);
    if let Some(start) = compact_started {
        if let Some(timing) = timing.as_deref_mut() {
            timing.compact_ns = start.elapsed().as_nanos() as u64;
            timing.compact_inst_capacity = mf.insts.capacity();
            timing.compact_cell_capacity = mf.cells.capacity();
            timing.compact_block_capacity = mf.blocks.capacity();
        }
    }

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
                return reject_candidate(&mut timing, "no_return");
            };
            if !arities.all(|arity| arity == ret_arity) {
                return reject_candidate(&mut timing, "nonuniform_return_arity");
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
        bounded: has_bounds,
        template,
        coalescable_params,
        forwardable_reads,
        forwards_reads: !has_lambda,
    }))
}

/// Leave forwarded loads allocated, preserving arena and emission IDs.
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
    if cache.census_enabled {
        cache.census.candidate_requests += 1;
        cache.census.candidate_requested_keys.push(inst.clone());
    }
    if !cache.cands.contains_key(inst) {
        if let Some(table) = cache.shared.as_ref() {
            if let Some(candidate) = table.lookup(inst) {
                if cache.census_enabled {
                    cache.census.candidate_cache_hits += 1;
                }
                cache.cands.insert(inst.clone(), candidate);
                return Ok(cache.cands[inst].as_deref());
            }
        }
        if cache.census_enabled {
            cache.census.candidate_derivations += 1;
            cache.census.candidate_keys.push(inst.clone());
            *cache
                .census
                .candidate_worker_derivations
                .entry(cache.worker.clone())
                .or_default() += 1;
        }
        let started = cache.census_enabled.then(Instant::now);
        let mut timing = InlineCandidateProfile::default();
        let result = classify(
            ws,
            scx,
            inst,
            concrete_plans,
            cache.census_enabled.then_some(&mut timing),
        );
        if let Some(start) = started {
            let elapsed = start.elapsed().as_nanos() as u64;
            cache.census.candidate_lower_ns += timing.lower_ns;
            cache.census.candidate_cleanup_ns += timing.cleanup_ns;
            cache.census.candidate_eligibility_ns += elapsed
                .saturating_sub(timing.lower_ns)
                .saturating_sub(timing.cleanup_ns)
                .saturating_sub(timing.full_clone_ns)
                .saturating_sub(timing.compact_ns);
        }
        let v = match result? {
            Some(candidate) => Some(Arc::new(candidate)),
            None => {
                if let Some(reason) = timing.rejection_reason {
                    cache.rejection(reason);
                }
                None
            }
        };
        cache.cands.insert(inst.clone(), v);
    } else if cache.census_enabled {
        cache.census.candidate_cache_hits += 1;
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
    scan_stats: &mut InlineScanStats,
) -> Result<Option<Site>, CompilerError> {
    let mut started = resume.is_none();
    if resume.is_some() {
        scan_stats.resume_cursor_progress += 1;
    }
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
        scan_stats.borrowed_block_scans += 1;
        scan_stats.instructions_scanned += insts.len().saturating_sub(start);
        for (idx, &iid) in insts.iter().enumerate().skip(start) {
            let (fref, args, dests) = match &func.insts[iid] {
                Inst::Call { func, args } => (*func, args, None),
                Inst::CallMulti { func, args, dests } => (*func, args, Some(dests)),
                _ => continue,
            };
            if fref == self_ref {
                continue;
            }
            scan_stats.call_instructions_examined += 1;
            let Some((inst, abi)) = call_target(func, iid, fref, ws)? else {
                cache.rejection("missing_or_incomplete_concrete_owner");
                continue;
            };
            scan_stats.call_target_resolutions += 1;
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
            let reason = if cand.subject != site.inst {
                Some("candidate_subject_mismatch")
            } else if cand.abi.as_ref() != site.abi.as_ref() {
                Some("candidate_abi_mismatch")
            } else if cand.func.params.len() != site.args.len()
                || site
                    .dests
                    .as_ref()
                    .map_or(cand.ret_arity > 1, |dests| cand.ret_arity != dests.len())
            {
                Some("candidate_arity_mismatch")
            } else if site.inst.args.is_empty()
                && cell_budget + cand.budget_cells() + cand.func.params.len() > CELL_BUDGET
                && cell_budget + cand.budget_cells() + cand.func.params.len()
                    - site_coalesced_cells(func, cand, &site, pinned)
                    > CELL_BUDGET
            {
                // A coalesced argument already occupies a counted caller cell.
                Some("cell_budget")
            } else {
                None
            };
            match reason {
                None => {
                    return Ok(Some(site));
                }
                Some(reason) => cache.rejection(reason),
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

/// Charge each callee parameter the splice copied (rather than coalesced onto
/// the argument's cell) to template forwarding, for the cleanup census.
fn mark_forwarded_params(caller: &mut MirFunc, callee: &MirFunc, scratch: &InlineScratch) {
    for param in &callee.params {
        if !scratch.overrides.contains_key(param) {
            caller.cells[scratch.cell_map[param]].producer = CellProducer::InlineParam;
        }
    }
}

/// Allocate every clone before remapping its operands.
fn clone_body(
    caller: &mut MirFunc,
    callee: &MirFunc,
    scratch: &mut InlineScratch,
    kind: Kind,
) -> CloneMapStats {
    caller.ensure_emit_ids();
    let (inst_base, cell_base, block_base) = caller.emit_nexts().unwrap();
    caller.insts.reserve(callee.insts.len());
    caller
        .cells
        .reserve(callee.cells.len().saturating_sub(scratch.overrides.len()));
    scratch.cell_map.clear();
    let cell_capacity = scratch.cell_map.capacity();
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
    mark_forwarded_params(caller, callee, scratch);
    let block_capacity = scratch.block_map.capacity();
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
    let inst_capacity = scratch.inst_map.capacity();
    scratch.inst_map.reserve(callee.insts.len());
    scratch.inst_ids.clear();
    scratch.inst_ids.reserve(callee.insts.len());
    for (iid, inst) in callee.insts.iter() {
        let nid =
            caller.alloc_inst_with_emit_id(inst.clone(), inst_base + callee.emit_inst_id(iid));
        scratch.inst_map.insert(iid, nid);
        scratch.inst_ids.push(nid);
    }
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
            let nbid = scratch.block_map[&bid];
            caller.blocks[nbid].insts = block.insts.iter().map(|i| scratch.inst_map[i]).collect();
            let mut term = block.term.clone();
            remap_term_clone(&mut term, &scratch.inst_map, &scratch.block_map);
            caller.blocks[nbid].term = term;
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

    CloneMapStats {
        inst_entries: scratch.inst_map.len(),
        cell_entries: scratch.cell_map.len(),
        block_entries: scratch.block_map.len(),
        inst_capacity: scratch.inst_map.capacity(),
        cell_capacity: scratch.cell_map.capacity(),
        block_capacity: if kind == Kind::General {
            scratch.block_map.capacity()
        } else {
            0
        },
        inst_growth: usize::from(scratch.inst_map.capacity() != inst_capacity),
        cell_growth: usize::from(scratch.cell_map.capacity() != cell_capacity),
        block_growth: usize::from(scratch.block_map.capacity() != block_capacity),
    }
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

fn remap_term_clone(
    term: &mut Term,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    term.remap_uses(&|x| im[&x]);
    term.remap_targets(&|x| bm[&x]);
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

/// Rename private hoisted definitions that collide in the caller.
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

/// Carry instruction metadata through the splice remap.
fn import_spec_bindings(
    func: &mut MirFunc,
    callee: &MirFunc,
    inst_map: &HashMap<Idx<Inst>, Idx<Inst>>,
    nested_names: &HashMap<Ustr, Ustr>,
) {
    for (old_id, key) in &callee.spec_bindings {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.spec_bindings.insert(new_id, key.clone());
        }
    }
    for (old_id, inst) in &callee.callee_instances {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.callee_instances.insert(new_id, inst.clone());
        }
    }
    for (old_id, plan) in &callee.call_abi_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.call_abi_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, plan) in &callee.call_rep_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.call_rep_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, &lanes) in &callee.flat_call_lanes {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.flat_call_lanes.insert(new_id, lanes);
        }
    }
    for (old_id, plan) in &callee.closure_env_plans {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.closure_env_plans.insert(new_id, plan.clone());
        }
    }
    for (old_id, tree) in &callee.resolved_bound_trees {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.resolved_bound_trees.insert(new_id, tree.clone());
        }
    }
    for (old_id, fref) in &callee.fn_value_symbols {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.fn_value_symbols.insert(new_id, *fref);
        }
    }
    for (old_id, identity) in &callee.code_value_identities {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.code_value_identities.insert(new_id, identity.clone());
        }
    }
    for (old_id, fingerprint) in &callee.lambda_abi_fingerprints {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.lambda_abi_fingerprints
                .insert(new_id, fingerprint.clone());
        }
    }
    for (old_id, representation) in &callee.lambda_abi_representations {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.lambda_abi_representations
                .insert(new_id, representation.clone());
        }
    }
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
    // Preserve the callee source location for diagnostics after inlining.
    for (old_id, span) in callee.inst_spans.iter() {
        if let Some(&new_id) = inst_map.get(&old_id) {
            func.inst_spans.insert(new_id, *span);
        }
    }
    for (old_id, ty) in &callee.inst_logical_types {
        if let Some(&new_id) = inst_map.get(old_id) {
            func.inst_logical_types.insert(new_id, *ty);
        }
    }
    // Intern each callee owner in the caller before remapping its sites.
    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, cs) in &callee.call_sites {
        let Some(&new_id) = inst_map.get(old_id) else {
            continue;
        };
        let owner = match owner_map.get(&cs.owner.0) {
            Some(&o) => o,
            None => {
                let Some(inst) = callee.site_owners.get(cs.owner.0 as usize) else {
                    continue;
                };
                let o = skull_mir::site::intern_owner(&mut func.site_owners, inst.clone());
                owner_map.insert(cs.owner.0, o);
                o
            }
        };
        func.call_sites.insert(
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
    census: &mut InlineScanStats,
) -> Result<(CloneMapStats, usize), CompilerError> {
    scratch.added_blocks.clear();
    let callee = &cand.func;
    let (bid, idx) = site.pos;
    let args = &site.args;
    let site_tree = func.bound_trees.remove(&site.call_id);
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id);
    let coalesced_cells = coalesce_overrides(func, cand, args, (bid, idx), pinned, scratch);
    let maps = clone_body(func, callee, scratch, cand.kind);
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
    import_spec_bindings(func, callee, &scratch.inst_map, &names);
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
        Kind::Straight => splice_straight(func, &site, cand, scratch, uses, &forwarded, census),
        Kind::General => splice_general(func, &site, cand, scratch, uses),
    }
    Ok((maps, coalesced_cells))
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

/// Inline within the caller block, preserving its terminator.
fn splice_straight(
    func: &mut MirFunc,
    site: &Site,
    cand: &CalleeBody,
    scratch: &InlineScratch,
    uses: &mut UseIndex,
    forwarded: &[(Idx<Inst>, Idx<Inst>)],
    census: &mut InlineScanStats,
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
            let binding_started = census.enabled.then(Instant::now);
            if to != call_id {
                let (instructions, terminators, replacements) = uses.replace(func, call_id, to);
                census.targeted_binding_users += instructions;
                census.targeted_binding_terminators += terminators;
                census.targeted_binding_replacements += replacements;
            }
            census.targeted_bindings += 1;
            if let Some(start) = binding_started {
                census.targeted_binding_ns += start.elapsed().as_nanos() as u64;
            }
        }
    }
}

/// Branch into the clone and send its returns to the continuation.
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
            func.cells[rc].producer = CellProducer::InlineResult;
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

/// Return the concrete splice dependencies needed for incremental emission.
pub(crate) fn inline_functions_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<(HashSet<FuncRef>, Vec<SpliceRecord>, InlineScanStats), CompilerError> {
    let mut spliced = HashSet::default();
    let mut records: Vec<SpliceRecord> = Vec::new();
    let mut scan_stats = InlineScanStats {
        enabled: cache.census_enabled,
        ..InlineScanStats::default()
    };
    cache.reset_census();
    if inline_disabled() {
        return Ok((spliced, records, scan_stats));
    }
    // The caller's top-level blocks (excludes closure sub-CFGs), maintained
    // incrementally so neither find_site nor the loop recomputes reachability.
    let top_start = cache.census_enabled.then(Instant::now);
    let top_blocks = top_level_blocks(func);
    if let Some(start) = top_start {
        cache.census.top_level_blocks_ns = start.elapsed().as_nanos() as u64;
        cache.census.top_level_blocks_population = top_blocks.len();
    }
    let mut top: HashSet<Idx<Block>> = top_blocks.into_iter().collect();
    // Caller cells a closure body writes; see [`pin_cells`].
    let mut pinned: HashSet<Idx<Cell>> = HashSet::default();
    pin_cells(func, &top, 0, &mut pinned);
    let mut uses: Option<UseIndex> = None;
    let mut original_calls: Option<HashSet<Idx<Inst>>> = None;
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
            &mut scan_stats,
        )?
        else {
            break;
        };
        let pos = site.pos;
        let fref = site.inst.func;
        if cache.census_enabled {
            scan_stats.spliced_keys.push(site.inst.clone());
        }
        let original_calls = original_calls.get_or_insert_with(|| {
            top.iter()
                .flat_map(|bid| func.blocks[*bid].insts.iter().copied())
                .filter(|iid| {
                    matches!(
                        &func.insts[*iid],
                        Inst::Call { .. } | Inst::CallMulti { .. }
                    )
                })
                .collect()
        });
        let transitive = !original_calls.contains(&site.call_id);
        if uses.is_none() {
            let started = cache.census_enabled.then(Instant::now);
            let built = UseIndex::build(func);
            if let Some(start) = started {
                cache.census.use_index_build_ns = start.elapsed().as_nanos() as u64;
                cache.census.use_index_values =
                    built.users.iter().filter(|sites| !sites.is_empty()).count();
            }
            uses = Some(built);
        }
        let uses = uses.as_mut().expect("built above");
        let argument_elements = site.args.len();
        let destination_elements = site.dests.as_ref().map_or(0, Vec::len);
        let prefix = pos.1;
        let tail = func.blocks[pos.0].insts.len().saturating_sub(pos.1 + 1);
        let (cands, census, scratch) = (&cache.cands, &mut cache.census, &mut cache.scratch);
        let cand = cands[&site.inst]
            .as_ref()
            .expect("site implies a cached candidate");
        let splice_start = cache.census_enabled.then(Instant::now);
        let arena_capacities = cache.census_enabled.then(|| {
            (
                func.insts.capacity(),
                func.cells.capacity(),
                func.blocks.capacity(),
            )
        });
        let result_cell = usize::from(cand.kind == Kind::General && site.dests.is_none());
        let blocks_before = func.blocks.len();
        let (maps, coalesced_cells) = splice(
            func,
            site,
            cand,
            ws,
            scx,
            concrete_plans,
            &pinned,
            scratch,
            uses,
            census,
        )?;
        let arena_reallocations = arena_capacities
            .map(|(inst_capacity, cell_capacity, block_capacity)| {
                (
                    usize::from(func.insts.capacity() > inst_capacity),
                    usize::from(func.cells.capacity() > cell_capacity),
                    usize::from(func.blocks.capacity() > block_capacity),
                )
            })
            .unwrap_or_default();
        top.extend(scratch.added_blocks.drain(..));
        pin_cells(func, &top, blocks_before, &mut pinned);
        let splice_ns = splice_start
            .map(|start| start.elapsed().as_nanos() as u64)
            .unwrap_or_default();
        scan_stats.splice_ns += splice_ns;
        records.push(SpliceRecord {
            callee: fref,
            straight: cand.kind == Kind::Straight,
            bounded: cand.bounded,
            transitive,
            callee_insts: cand.insts,
            clone: full_clone_stats(
                cand,
                cand.kind == Kind::Straight,
                argument_elements,
                destination_elements,
                prefix,
                tail,
                maps,
                coalesced_cells,
                arena_reallocations,
            ),
            splice_ns,
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
    // Most bodies own no statics at all, and this runs for every emitted fn --
    // so the set is only built when there is something that could duplicate.
    if func.statics.len() > 1 {
        let mut seen: HashSet<ustr::Ustr> = HashSet::default();
        func.statics.retain(|sc| seen.insert(sc.cell));
    }
    #[cfg(test)]
    if let Some(uses) = &uses {
        uses.record_drift(func);
    }
    scan_stats.add(std::mem::take(&mut cache.census));
    for record in &records {
        if record.straight {
            scan_stats.straight_accepted += 1;
        } else {
            scan_stats.general_accepted += 1;
        }
        if record.transitive {
            scan_stats.transitive_splices += 1;
        } else {
            scan_stats.direct_splices += 1;
        }
    }
    Ok((spliced, records, scan_stats))
}

fn fn_has_bounds(f: &bone_core::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

/// Report any unresolved bound calls before rendering.
pub fn validate_no_residual_bound_calls(
    ws: &Workspace,
    f: &bone_core::hir::Function,
    mf: &MirFunc,
) -> Result<(), CompilerError> {
    // Walk block inst-lists, not the raw arena: a splice drops the consumed call
    // inst from its block but the arena still holds it.
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
    fn env_switch_turns_inlining_off_only_when_set_on() {
        assert!(env_disables_inline(Some("1")));
        assert!(env_disables_inline(Some("true")));
        assert!(!env_disables_inline(Some("0")));
        assert!(!env_disables_inline(Some("")));
        assert!(!env_disables_inline(None));
    }

    /// Issue #5: `bound::repack_bound_next_element` allocates the element's
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
        let graph = skull_graph::instance_graph::build_in(
            &ws,
            vec![top],
            false,
            std::sync::Arc::clone(&interner),
        );
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

    fn candidate_test_key() -> Instance {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                "fn candidate() -> int { return 1 }",
                Ustr::from("<candidate-table-test>"),
            )
            .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<candidate-table-test>"))
            .expect("module");
        let (func, _) = ws.module(mid).functions.iter().next().expect("candidate");
        Instance::base(FuncRef { module: mid, func })
    }

    fn candidate_warm(key: Instance, rejection: Option<&'static str>) -> InlineCandidateWarm {
        InlineCandidateWarm {
            key,
            candidate: None,
            profile: InlineCandidateProfile {
                eligible: rejection.is_none(),
                rejected_before_lower: rejection.is_some(),
                rejection_reason: rejection,
                worker: "candidate-table-test".to_string(),
                ..InlineCandidateProfile::default()
            },
        }
    }

    #[test]
    fn incremental_candidate_replacement_updates_rejection_profile() {
        let key = candidate_test_key();
        let base = Arc::new(InlineCandidateTable::from_warms_with_base(
            None,
            vec![key.clone()],
            vec![candidate_warm(key.clone(), None)],
        ));
        assert_eq!(base.stats().eligible, 1);
        assert_eq!(base.stats().rejected, 0);

        let delta = InlineCandidateTable::from_warms_incremental(
            Arc::clone(&base),
            vec![candidate_warm(key.clone(), Some("body_changed"))],
            &[],
            &[],
        );
        let stats = delta.stats();
        assert_eq!(stats.eligible, 0, "the replaced candidate stayed eligible");
        assert_eq!(
            stats.rejected, 1,
            "the replaced rejection was not published"
        );
        assert_eq!(stats.rejection_reasons.get("body_changed"), Some(&1));
        assert!(
            !stats.profiles[&key].eligible,
            "the replacement profile is stale"
        );
    }

    #[test]
    fn lower_times_by_key_are_kept_only_for_a_census() {
        let key = candidate_test_key();
        let lowered = |worker: &str| {
            let mut warm = candidate_warm(key.clone(), None);
            warm.profile.lowered = true;
            warm.profile.lower_ns = 7;
            warm.profile.worker = worker.to_string();
            warm
        };
        let plain =
            InlineCandidateTable::from_warms_with_base(None, vec![key.clone()], vec![lowered("")]);
        assert_eq!(plain.stats().lower_ns, 7);
        assert!(plain.stats().lower_ns_by_key.is_empty());
        let census = InlineCandidateTable::from_warms_with_base(
            None,
            vec![key.clone()],
            vec![lowered("census")],
        );
        assert_eq!(census.stats().lower_ns_by_key.get(&key), Some(&7));
    }
}
