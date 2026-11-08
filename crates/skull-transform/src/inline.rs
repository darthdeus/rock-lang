//! Function inlining: a MIR->MIR pass that splices an eligible callee's body
//! into the caller so a hot loop traces with no per-call frame. Re-fit of the
//! deleted hybrid-SSA `skull/src/inline.rs` onto the cell-model CFG.
//!
//! The cell model makes the splice simpler than the old SSA one: there are NO
//! block params, so arg-binding is a `Store` into the callee's (cloned) param
//! cell and a returned value is a `Store` into a result/dest cell — the caller's
//! `cleanup` (copy-prop + DCE) then elides the round-trips.
//!
//! Policy is ANNOTATION-GATED: only `@inline`/`@pure` functions inline. This
//! keeps the hot-reload re-emit
//! fan-out + overlay small (a body edit re-emits every fn that inlined the
//! changed callee, so fewer inlines = fewer dependents). The hot stdlib helpers
//! (math, getters, value-struct ctors) carry `@inline`/`@pure`, so they still
//! inline; incidental small accessors no longer do.

use bone_core::prelude::Instant;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::Cell as ThreadCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use bone_core::prelude::{BoundTree, CompilerError};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::concrete::ConcretePlans;
use skull_mir::instance::Instance;
use skull_mir::lower::{BackendFnAbiPlan, BackendFnAbiPlanHandle, ScalarCtx};
use skull_mir::mir::*;
use ustr::Ustr;

use crate::top_level_blocks;

/// Per-caller splice budget — bounds mutual-recursion expansion. A remaining
/// real call is always correct.
const MAX_INLINES_PER_FN: usize = 1000;
/// Max cells a caller may hold before inlining stops (the call stays — always
/// correct). This is a RAW cell count, but skull-codegen-lua now COLORS cells
/// (liveness-coalesced onto Lua's 200-local cap), so a function with N raw cells
/// emits far fewer locals (e.g. 193 raw -> 28 colored). The
/// budget therefore guards against runaway expansion / the rare all-simultaneously
/// -live pathological case, not the typical local count; 400 matches the value
/// the old (coloring-aware) skull validated across the corpus.
const CELL_BUDGET: usize = 400;

thread_local! {
    static INLINE_OFF: ThreadCell<bool> = const { ThreadCell::new(false) };
}

pub struct InlineOffGuard {
    previous: bool,
}

pub fn inline_off_guard() -> InlineOffGuard {
    let previous = INLINE_OFF.with(|off| {
        let previous = off.get();
        off.set(true);
        previous
    });
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

fn inline_disabled() -> bool {
    inline_off_active()
}

/// Whether a candidate callee is a single straight-line block or a CFG.
#[derive(Copy, Clone, PartialEq)]
enum Kind {
    Straight,
    General,
}

/// A pristine lowered snapshot of an eligible callee — every splice clones from
/// this clean body (transitive inlining re-scans the caller, never mutates a
/// candidate).
#[derive(Clone)]
struct CalleeBody {
    func: MirFunc,
    /// The raw lowered body shared with emission consumers. Every mutating
    /// consumer clones it; only this table owns the canonical source.
    canonical: Arc<MirFunc>,
    /// The concrete body this snapshot was lowered from. A declaration-level
    /// snapshot is not a valid source for a concrete splice.
    subject: Instance,
    /// The one ABI plan both the snapshot and its call site must use.
    abi: BackendFnAbiPlanHandle,
    kind: Kind,
    /// Uniform `Return` value count (0 for void, 1 for a scalar, N for a
    /// scalarized value struct). Matched against the call site's result shape.
    ret_arity: usize,
    /// The callee's reachable blocks (general candidates only — the straight path
    /// clones no blocks). After a general splice their clones become reachable
    /// top-level blocks of the caller; the driver adds them to its top-level set.
    reachable: Vec<Idx<Block>>,
    /// Reachable inst count of the (cleaned) body — the code each splice copies.
    /// Diagnostic only (feeds the inline census); not used by the policy.
    insts: usize,
    /// Iterator-bounded body, which has no standalone placeholder-bearing emit.
    bounded: bool,
    /// Source/live arena and side-table populations. The first census uses the
    /// live closure to expose dead cloning; the template cut later reuses this
    /// same closure as its canonical clone source.
    template: InlineCloneStats,
    /// Param cells this callee NEVER whole-`Store`s (only reads / `Field` /
    /// `StoreField`). Their arg can be COALESCED — an arg that is a plain
    /// `Load(c)` binds the cloned param directly to the caller cell `c` instead
    /// of copying `Store(param, Load(c))`. Removing the whole-cell copy is what
    /// lets a mutated-by-ref cursor struct stay a SINGLE cell so
    /// `scalarize_struct_cells` (SROA) can peel it (the zero-alloc iterator
    /// path). Sound because field-writeback through a by-ref arg is already the
    /// language semantics, and a param the callee reassigns whole is excluded
    /// (aliasing it to the caller cell would clobber the caller's variable).
    coalescable_params: Vec<bool>,
}

/// One splice the inliner performed, for the inline census
/// (`inline_functions_with_plans` returns these alongside its dependency set). Records the
/// callee whose body was copied in, its shape, and how much code it added.
pub struct SpliceRecord {
    pub callee: FuncRef,
    pub straight: bool,
    pub bounded: bool,
    pub transitive: bool,
    /// Reachable inst count of the callee body copied at this site.
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
    pub coalesced_cells: usize,
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
    pub substitution_invocations: usize,
    pub substitution_instructions_inspected: usize,
    pub substitution_terminators_inspected: usize,
    pub substitution_replacements: usize,
    pub substitution_ns: u64,
    pub use_index_build_ns: u64,
    pub use_index_values: usize,
    pub targeted_bindings: usize,
    pub targeted_binding_users: usize,
    pub targeted_binding_terminators: usize,
    pub targeted_binding_replacements: usize,
    pub targeted_binding_ns: u64,
    /// Cleanup Load/Store attribution by cell producer, filled by the emit
    /// driver around each post-inline cleanup when the census is enabled.
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
        self.coalesced_cells += other.coalesced_cells;
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
        self.substitution_invocations += other.substitution_invocations;
        self.substitution_instructions_inspected += other.substitution_instructions_inspected;
        self.substitution_terminators_inspected += other.substitution_terminators_inspected;
        self.substitution_replacements += other.substitution_replacements;
        self.substitution_ns += other.substitution_ns;
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

/// Lazy callee-eligibility cache, keyed by [`FuncRef`]. `None` = looked up and
/// not inlinable. Lives across a whole-workspace emit so each callee lowers +
/// classifies once -- and, in the incremental emit, ACROSS body edits: a callee
/// whose body and scalar ABI are unchanged keeps its cached lowered body instead
/// of re-lowering every edit. The driver [`invalidate`](Self::invalidate)s the
/// affected callees per edit so no stale body is ever cloned.
/// Cache key: the callee [`Instance`] -- the same concrete-body identity the
/// chunk cache and both emitters key on. Each distinct instantiation gets its
/// own snapshot, so compile cost scales with element types used, not call sites.
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
    pub rejection_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineCandidateTableStats {
    pub keys: usize,
    pub warmed: usize,
    pub reused: usize,
    pub eligible: usize,
    pub rejected: usize,
    pub lower_attempted: usize,
    pub lowered: usize,
    pub rejected_before_lower: usize,
    pub lower_ns: u64,
    pub full_clone_ns: u64,
    pub cleanup_ns: u64,
    pub compact_ns: u64,
    pub full_clone_inst_capacity: usize,
    pub full_clone_cell_capacity: usize,
    pub full_clone_block_capacity: usize,
    pub compact_inst_capacity: usize,
    pub compact_cell_capacity: usize,
    pub compact_block_capacity: usize,
    pub eligibility_ns: u64,
    pub raw_instructions: usize,
    pub raw_cells: usize,
    pub raw_blocks: usize,
    pub cleaned_instructions: usize,
    pub cleaned_cells: usize,
    pub cleaned_blocks: usize,
    pub profiles: BTreeMap<Instance, InlineCandidateProfile>,
    pub lower_ns_by_key: BTreeMap<Instance, u64>,
    pub worker_derivations: BTreeMap<String, usize>,
    pub rejection_reasons: BTreeMap<String, usize>,
}

pub struct InlineCandidateWarm {
    key: CacheKey,
    candidate: Option<Arc<CalleeBody>>,
    pub lower_ns: u64,
    pub full_clone_ns: u64,
    pub cleanup_ns: u64,
    pub compact_ns: u64,
    pub lower_attempted: bool,
    pub lowered: bool,
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
    pub eligibility_ns: u64,
    pub rejection: Option<&'static str>,
    worker: String,
}

impl InlineCandidateProfile {
    fn from_warm(warm: &InlineCandidateWarm) -> Self {
        Self {
            lower_ns: warm.lower_ns,
            full_clone_ns: warm.full_clone_ns,
            cleanup_ns: warm.cleanup_ns,
            compact_ns: warm.compact_ns,
            eligibility_ns: warm.eligibility_ns,
            lower_attempted: warm.lower_attempted,
            lowered: warm.lowered,
            eligible: warm.rejection.is_none(),
            rejected_before_lower: warm.rejection.is_some() && !warm.lower_attempted,
            raw_instructions: warm.raw_instructions,
            raw_cells: warm.raw_cells,
            raw_blocks: warm.raw_blocks,
            cleaned_instructions: warm.cleaned_instructions,
            cleaned_cells: warm.cleaned_cells,
            cleaned_blocks: warm.cleaned_blocks,
            full_clone_inst_capacity: warm.full_clone_inst_capacity,
            full_clone_cell_capacity: warm.full_clone_cell_capacity,
            full_clone_block_capacity: warm.full_clone_block_capacity,
            compact_inst_capacity: warm.compact_inst_capacity,
            compact_cell_capacity: warm.compact_cell_capacity,
            compact_block_capacity: warm.compact_block_capacity,
            worker: warm.worker.clone(),
            rejection_reason: warm.rejection.map(str::to_string),
        }
    }
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
    macro_rules! adjust_bool {
        ($field:ident) => {
            if add {
                stats.$field += usize::from(profile.$field);
            } else {
                stats.$field = stats.$field.saturating_sub(usize::from(profile.$field));
            }
        };
    }
    adjust!(lower_ns);
    adjust!(full_clone_ns);
    adjust!(cleanup_ns);
    adjust!(compact_ns);
    adjust!(eligibility_ns);
    adjust_bool!(lower_attempted);
    adjust_bool!(lowered);
    adjust_bool!(rejected_before_lower);
    adjust!(raw_instructions);
    adjust!(raw_cells);
    adjust!(raw_blocks);
    adjust!(cleaned_instructions);
    adjust!(cleaned_cells);
    adjust!(cleaned_blocks);
    adjust!(full_clone_inst_capacity);
    adjust!(full_clone_cell_capacity);
    adjust!(full_clone_block_capacity);
    adjust!(compact_inst_capacity);
    adjust!(compact_cell_capacity);
    adjust!(compact_block_capacity);
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
    if let Some(reason) = &profile.rejection_reason {
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
        let census_enabled = warms.iter().any(|warm| !warm.worker.is_empty())
            || base.is_some_and(|table| !table.stats.profiles.is_empty());
        if census_enabled {
            if let Some(base) = base {
                stats.profiles = base.stats.profiles.clone();
            }
        }
        for warm in warms {
            stats.lower_ns += warm.lower_ns;
            stats.full_clone_ns += warm.full_clone_ns;
            stats.cleanup_ns += warm.cleanup_ns;
            stats.compact_ns += warm.compact_ns;
            stats.full_clone_inst_capacity += warm.full_clone_inst_capacity;
            stats.full_clone_cell_capacity += warm.full_clone_cell_capacity;
            stats.full_clone_block_capacity += warm.full_clone_block_capacity;
            stats.compact_inst_capacity += warm.compact_inst_capacity;
            stats.compact_cell_capacity += warm.compact_cell_capacity;
            stats.compact_block_capacity += warm.compact_block_capacity;
            stats.raw_instructions += warm.raw_instructions;
            stats.raw_cells += warm.raw_cells;
            stats.raw_blocks += warm.raw_blocks;
            stats.cleaned_instructions += warm.cleaned_instructions;
            stats.cleaned_cells += warm.cleaned_cells;
            stats.cleaned_blocks += warm.cleaned_blocks;
            if warm.lower_attempted {
                stats.lower_attempted += 1;
            }
            if warm.lowered {
                stats.lowered += 1;
                stats
                    .lower_ns_by_key
                    .insert(warm.key.clone(), warm.lower_ns);
            }
            stats.eligibility_ns += warm.eligibility_ns;
            if !warm.worker.is_empty() {
                *stats
                    .worker_derivations
                    .entry(warm.worker.clone())
                    .or_default() += 1;
            }
            if let Some(reason) = warm.rejection {
                stats.rejected += 1;
                if !warm.lower_attempted {
                    stats.rejected_before_lower += 1;
                }
                *stats
                    .rejection_reasons
                    .entry(reason.to_string())
                    .or_default() += 1;
            } else {
                stats.eligible += 1;
            }
            if census_enabled {
                stats
                    .profiles
                    .insert(warm.key.clone(), InlineCandidateProfile::from_warm(&warm));
            }
            let previous = warmed.insert(warm.key, warm.candidate);
            assert!(
                previous.is_none(),
                "inline candidate table received a duplicate key"
            );
        }
        if census_enabled {
            stats.lower_ns_by_key = stats
                .profiles
                .iter()
                .filter_map(|(key, profile)| {
                    profile.lowered.then_some((key.clone(), profile.lower_ns))
                })
                .collect();
        }
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

    /// Publish an edit-local candidate delta over the prior immutable table.
    /// Unchanged entries stay in the parent; only invalidated, added, and
    /// removed identities occupy this layer.
    pub fn from_warms_incremental(
        base: Arc<Self>,
        warms: Vec<InlineCandidateWarm>,
        added: &[Instance],
        removed: &[Instance],
    ) -> Self {
        let mut entries = HashMap::default();
        let mut stats = base.stats.clone();
        let census_enabled =
            !stats.profiles.is_empty() || warms.iter().any(|warm| !warm.worker.is_empty());
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
                let profile = InlineCandidateProfile::from_warm(warm);
                adjust_candidate_profile(&mut stats, &warm.key, &profile, true);
                stats.profiles.insert(warm.key.clone(), profile);
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

    /// Whether a previously residual call remains a candidate with the same
    /// candidate-side eligibility facts after a body edit. `self` is the prior
    /// table and `other` is its replacement. Only two eligible entries can
    /// prove stability; missing or rejected entries conservatively return
    /// `false`. Body contents may differ because callers track embedded bodies
    /// separately when an earlier inline pass spliced them.
    pub fn residual_eligibility_matches(&self, other: &Self, key: &Instance) -> bool {
        let (Some(Some(before)), Some(Some(after))) = (self.lookup(key), other.lookup(key)) else {
            return false;
        };

        before.subject == after.subject
            && before.abi == after.abi
            && before.func.params.len() == after.func.params.len()
            && before.ret_arity == after.ret_arity
            && before.template.source_cells == after.template.source_cells
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

/// Exact value-use ownership for one caller. The inliner changes a small set of
/// instructions per splice, so keeping the users lets straight-result binding
/// rewrite cross-block uses without rescanning the caller arena.
#[derive(Default)]
struct UseIndex {
    users: HashMap<Idx<Inst>, Vec<UseSite>>,
    affected: HashSet<UseSite>,
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
                .flat_map(|(value, sites)| sites.iter().map(move |site| (*value, *site)))
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

    fn add_use(&mut self, value: Idx<Inst>, site: UseSite) {
        let users = self.users.entry(value).or_default();
        if !users.contains(&site) {
            users.push(site);
        }
    }

    fn remove_use(&mut self, value: Idx<Inst>, site: UseSite) {
        if let Some(users) = self.users.get_mut(&value) {
            users.retain(|user| *user != site);
            if users.is_empty() {
                self.users.remove(&value);
            }
        }
    }

    fn add_inst(&mut self, func: &MirFunc, iid: Idx<Inst>) {
        func.insts[iid].uses_each(&mut |value| {
            self.add_use(value, UseSite::Inst(iid));
        });
    }

    /// Index every instruction allocated at or after arena position `mark`.
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

    /// Replace one value in its exact users. The returned counts are attribution
    /// only: unlike the removed `subst_all`, they cover users that actually had
    /// to be touched.
    fn replace(
        &mut self,
        func: &mut MirFunc,
        from: Idx<Inst>,
        to: Idx<Inst>,
    ) -> (usize, usize, usize) {
        if from == to {
            return (0, 0, 0);
        }
        let sites = self.users.remove(&from).unwrap_or_default();
        self.affected.clear();
        let mut instructions = 0;
        let mut terminators = 0;
        let replacements = ThreadCell::new(0usize);
        for site in sites {
            if !self.affected.insert(site) {
                continue;
            }
            match site {
                UseSite::Inst(iid) => {
                    self.remove_inst(func, iid);
                    crate::remap_inst(&mut func.insts[iid], &|value| {
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
                    crate::remap_term(&mut func.blocks[bid].term, &|value| {
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
        let mut timing = CandidateTiming::default();
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
        let rejection = timing.rejection;
        Ok(InlineCandidateWarm {
            key: inst,
            candidate,
            lower_ns: timing.lower_ns,
            full_clone_ns: timing.full_clone_ns,
            cleanup_ns: timing.cleanup_ns,
            compact_ns: timing.compact_ns,
            lower_attempted: timing.lower_attempted,
            lowered: timing.lowered,
            raw_instructions: timing.raw_instructions,
            raw_cells: timing.raw_cells,
            raw_blocks: timing.raw_blocks,
            cleaned_instructions: timing.cleaned_instructions,
            cleaned_cells: timing.cleaned_cells,
            cleaned_blocks: timing.cleaned_blocks,
            full_clone_inst_capacity: timing.full_clone_inst_capacity,
            full_clone_cell_capacity: timing.full_clone_cell_capacity,
            full_clone_block_capacity: timing.full_clone_block_capacity,
            compact_inst_capacity: timing.compact_inst_capacity,
            compact_cell_capacity: timing.compact_cell_capacity,
            compact_block_capacity: timing.compact_block_capacity,
            eligibility_ns,
            rejection,
            worker: self.worker.clone(),
        })
    }

    /// Drop every local cached candidate for `fref` so the next lookup consumes
    /// the replacement shared snapshot. A fref's
    /// cached body goes stale iff its own body changed OR a fn it calls flipped
    /// scalar ABI -- both are within the emit's `affected` set, which the
    /// incremental driver invalidates wholesale. A body edit must also drop the
    /// specialized snapshots keyed off it, so this matches on the `fref` component.
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

fn forced_cells(func: &MirFunc) -> Vec<(u32, u32)> {
    let top: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
    let mut cells = HashSet::default();
    for (_, inst) in func.insts.iter() {
        if let Inst::Lambda { params, entry } = inst {
            for &cell in params {
                cells.insert((cell, *entry));
            }
        }
    }
    for (bid, block) in func.blocks.iter() {
        if top.contains(&bid) {
            continue;
        }
        for &iid in &block.insts {
            func.insts[iid].cells_each(&mut |cell| {
                cells.insert((cell, bid));
            });
        }
    }
    let mut cells: Vec<(u32, u32)> = cells
        .into_iter()
        .map(|(cell, block)| (func.emit_cell_id(cell), func.emit_block_id(block)))
        .collect();
    cells.sort_unstable();
    cells
}

fn live_closure(func: &MirFunc) -> LiveClosure {
    let mut live = LiveClosure::default();
    let mut blocks = vec![func.entry];
    let mut insts = Vec::new();
    while let Some(bid) = blocks.pop() {
        if !live.blocks.insert(bid) {
            continue;
        }
        let block = &func.blocks[bid];
        insts.extend(block.insts.iter().copied());
        block.term.uses_each(&mut |iid| insts.push(iid));
        skull_mir::cfg::successors(&block.term, |target| blocks.push(target));
    }
    for param in &func.params {
        live.cells.insert(*param);
    }
    while let Some(iid) = insts.pop() {
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
        while let Some(bid) = blocks.pop() {
            if !live.blocks.insert(bid) {
                continue;
            }
            let block = &func.blocks[bid];
            insts.extend(block.insts.iter().copied());
            block.term.uses_each(&mut |value| insts.push(value));
            skull_mir::cfg::successors(&block.term, |target| blocks.push(target));
        }
    }
    if let Some(cell) = func.resume_arg {
        live.cells.insert(cell);
    }
    for (flag, block) in &func.coro_defers {
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

/// Cut one canonical pristine candidate down to the closure consumed by a
/// splice. Arena IDs are compacted once here; every later splice remaps from
/// this template and never pays for source entries outside the live closure.
fn compact_live_template(
    mut source: MirFunc,
    live: &LiveClosure,
) -> (MirFunc, HashMap<Idx<Block>, Idx<Block>>) {
    let source_inst_span = source.emit_inst_span();
    let source_cell_span = source.emit_cell_span();
    let source_block_span = source.emit_block_span();
    let source_forced_cells = forced_cells(&source);
    let mut cleanup_load_cells = HashSet::default();
    for (bid, block) in source.blocks.iter() {
        if live.blocks.contains(&bid) {
            continue;
        }
        for &iid in &block.insts {
            if let Inst::Load(cell) = source.insts[iid] {
                cleanup_load_cells.insert(cell);
            }
        }
    }
    let mut cell_map = HashMap::default();
    let mut cells = comfy_la_arena::Arena::with_capacity(live.cells.len());
    for (id, cell) in source.cells.iter() {
        if live.cells.contains(&id) {
            cell_map.insert(id, cells.alloc(cell.clone()));
        }
    }

    let mut block_map = HashMap::default();
    let mut blocks = comfy_la_arena::Arena::with_capacity(live.blocks.len());
    for (id, _) in source.blocks.iter() {
        if live.blocks.contains(&id) {
            block_map.insert(id, blocks.alloc(Block::new()));
        }
    }

    let mut inst_map = HashMap::default();
    let mut insts = comfy_la_arena::Arena::with_capacity(live.insts.len());
    for (id, inst) in source.insts.iter() {
        if live.insts.contains(&id) {
            inst_map.insert(id, insts.alloc(inst.clone()));
        }
    }
    for id in &live.insts {
        let new_id = inst_map[id];
        remap_clone(&mut insts[new_id], &inst_map, &cell_map, &block_map);
    }
    for id in live.blocks.iter().copied() {
        let new_id = block_map[&id];
        let old = &source.blocks[id];
        blocks[new_id].insts = old.insts.iter().map(|inst| inst_map[inst]).collect();
        let mut term = old.term.clone();
        remap_term_clone(&mut term, &inst_map, &block_map);
        blocks[new_id].term = term;
    }

    let mut emit_insts = vec![0; insts.len()];
    for (old, new) in &inst_map {
        emit_insts[u32::from(new.into_raw()) as usize] = source.emit_inst_id(*old);
    }
    let mut emit_cells = vec![0; cells.len()];
    for (old, new) in &cell_map {
        emit_cells[u32::from(new.into_raw()) as usize] = source.emit_cell_id(*old);
    }
    let mut emit_blocks = vec![0; blocks.len()];
    for (old, new) in &block_map {
        emit_blocks[u32::from(new.into_raw()) as usize] = source.emit_block_id(*old);
    }

    let mut template = MirFunc::new(source.name);
    template.code_identity = source.code_identity.clone();
    template.code_abi_fingerprint = source.code_abi_fingerprint.clone();
    template.code_compatibility = source.code_compatibility.clone();
    template.lambda_abi_fingerprints = source
        .lambda_abi_fingerprints
        .iter()
        .filter_map(|(old_id, fingerprint)| {
            inst_map
                .get(old_id)
                .map(|new_id| (*new_id, fingerprint.clone()))
        })
        .collect();
    template.lambda_abi_representations = source
        .lambda_abi_representations
        .iter()
        .filter_map(|(old_id, representation)| {
            inst_map
                .get(old_id)
                .map(|new_id| (*new_id, representation.clone()))
        })
        .collect();
    template.nested_fn_identities = source.nested_fn_identities.clone();
    template.rep_arena = source.rep_arena.clone();
    template.params = source.params.iter().map(|param| cell_map[param]).collect();
    template.lower_timing_enabled = source.lower_timing_enabled;
    template.lower_attribution = source.lower_attribution.clone();
    template.abi_plan = source.abi_plan.clone();
    template.abi_representation = source.abi_representation.clone();
    template.global_plan = source.global_plan.clone();
    template.statics = source.statics.clone();
    template.comptime_reads = source.comptime_reads.clone();
    template.struct_registry_roots = source.struct_registry_roots.clone();
    template.insts = insts;
    template.cells = cells;
    template.blocks = blocks;
    let mut cleanup_load_cells: Vec<_> = cleanup_load_cells
        .into_iter()
        .filter_map(|cell| cell_map.get(&cell).copied())
        .collect();
    cleanup_load_cells.sort_unstable_by_key(|cell| u32::from(cell.into_raw()));
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
    template.entry = block_map[&source.entry];
    template.is_coroutine = source.is_coroutine;
    template.resume_arg = source.resume_arg.map(|cell| cell_map[&cell]);
    template.coro_defers = source
        .coro_defers
        .iter()
        .filter(|&(_, block)| live.blocks.contains(block))
        .map(|(flag, block)| (cell_map[flag], block_map[block]))
        .collect();
    template.for_iter_headers = source
        .for_iter_headers
        .iter()
        .filter(|header| live.blocks.contains(header))
        .map(|header| block_map[header])
        .collect();
    template.hoisted_fn_globals = source.hoisted_fn_globals.clone();

    macro_rules! remap_inst_table {
        ($field:ident) => {
            for (old_id, value) in &source.$field {
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
    for (param, values) in &source.param_default_values {
        let values = values
            .iter()
            .filter_map(|value| inst_map.get(value).copied())
            .collect::<Vec<_>>();
        if !values.is_empty() {
            template.param_default_values.insert(*param, values);
        }
    }
    for (old_id, identity) in &source.code_value_identities {
        if let Some(&new_id) = inst_map.get(old_id) {
            template
                .code_value_identities
                .insert(new_id, identity.clone());
        }
    }
    for (old_id, plan) in &source.closure_env_plans {
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
    for old_id in &source.bound_next_split {
        if let Some(&new_id) = inst_map.get(old_id) {
            template.bound_next_split.insert(new_id);
        }
    }
    for (old_id, span) in source.inst_spans.iter() {
        if let Some(&new_id) = inst_map.get(&old_id) {
            template.inst_spans.insert(new_id, *span);
        }
    }

    let mut owner_map: HashMap<u32, skull_mir::site::OwnerRef> = HashMap::default();
    for (old_id, site) in &source.call_sites {
        let Some(&new_id) = inst_map.get(old_id) else {
            continue;
        };
        let owner = match owner_map.get(&site.owner.0) {
            Some(&owner) => owner,
            None => {
                let owner = source
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

    let nested = std::mem::take(&mut source.nested_fns);
    template.nested_fns = nested
        .into_iter()
        .map(|nested| {
            let nested_live = live_closure(&nested);
            compact_live_template(nested, &nested_live).0
        })
        .collect();
    (template, block_map)
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
    let Some(source) = callee.emit_ids.as_ref() else {
        return Vec::new();
    };
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
    stats.coalesced_cells = coalesced_cells;
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

#[derive(Default)]
struct CandidateTiming {
    lower_ns: u64,
    full_clone_ns: u64,
    cleanup_ns: u64,
    compact_ns: u64,
    lower_attempted: bool,
    lowered: bool,
    raw_instructions: usize,
    raw_cells: usize,
    raw_blocks: usize,
    cleaned_instructions: usize,
    cleaned_cells: usize,
    cleaned_blocks: usize,
    full_clone_inst_capacity: usize,
    full_clone_cell_capacity: usize,
    full_clone_block_capacity: usize,
    compact_inst_capacity: usize,
    compact_cell_capacity: usize,
    compact_block_capacity: usize,
    rejection: Option<&'static str>,
}

fn reject_candidate(
    timing: &mut Option<&mut CandidateTiming>,
    reason: &'static str,
) -> Result<Option<CalleeBody>, CompilerError> {
    if let Some(timing) = timing.as_deref_mut() {
        timing.rejection = Some(reason);
    }
    Ok(None)
}

/// Lower `fref` and classify it as an inline candidate. Hard exclusions: extern,
/// `inline(lua)`, `co fn`, lambda-bearing, self-recursive, or reachable `Yield`.
/// General splices preserve `Unreachable` traps and route only `Return` to the
/// caller continuation.
fn classify(
    ws: &Workspace,
    scx: &ScalarCtx,
    inst: &Instance,
    concrete_plans: &ConcretePlans,
    mut timing: Option<&mut CandidateTiming>,
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
    // An `Iterator`-bounded body carries placeholder calls until a concrete
    // splice or standalone demand resolves them, so inline eligibility forces
    // the optimization when the body is otherwise eligible.
    let has_bounds = f.generic_bounds.iter().any(Option::is_some);
    if !annotation_gate(has_inline, has_pure) && !has_bounds {
        return reject_candidate(&mut timing, "missing_inline_annotation");
    }
    // A snapshot lowers the body as if hand-written for the concrete
    // instantiation; a non-generic instance's is the
    // declaration's own body.
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
    // Cleanup first so the size gate reflects emitted size and the splice clones
    // a tight body (the caller re-runs cleanup over the whole result anyway).
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
    // Structural exclusions over the whole arena.
    let mut has_lambda = false;
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Lambda { .. } => {
                // A lambda's sub-CFG + captured cells only clone through the
                // block-cloning General splice. A bounded fn has no standalone
                // emission, so refusing it would leave the caller a residual
                // bounded call (an emit error); force it through General below.
                // Non-bounded fns keep the v0 refusal (they have a real
                // emission to fall back on).
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
    // Yield requires coroutine state that an ordinary caller cannot supply.
    // Unreachable is a terminal trap, including an exhaustive match's fallback;
    // the general splice preserves it without routing it to the continuation.
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

    // A param is coalescable iff nothing in the body rebinds its cell WHOLE (a
    // `Store` to it, or defining it as a multi-return dest / map cursor). Reads
    // and `Field`/`StoreField` don't count — those are exactly what coalescing
    // preserves (field-writeback through the shared cell).
    let mut rebound: HashSet<Idx<Cell>> = HashSet::default();
    for (_, inst) in mf.insts.iter() {
        match inst {
            Inst::Store { cell, .. } => {
                rebound.insert(*cell);
            }
            Inst::CallMulti { dests, .. }
            | Inst::CallValueMulti { dests, .. }
            | Inst::CallBoundMulti { dests, .. } => {
                rebound.extend(dests.iter().copied());
            }
            Inst::MapNext {
                key_cell, val_cell, ..
            } => {
                rebound.insert(*key_cell);
                rebound.insert(*val_cell);
            }
            _ => {}
        }
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

    // A lambda-bearing body must take the General splice even when its entry
    // ends in Return: the straight path clones no blocks, so the lambda's
    // sub-CFG (referenced by `Inst::Lambda { entry }`) would dangle.
    if !has_lambda && matches!(mf.blocks[mf.entry].term, Term::Return(_)) {
        // Straight: entry ends in Return, so it is the only reachable block.
        let ret_arity = match &mf.blocks[mf.entry].term {
            Term::Return(vs) => vs.len(),
            _ => unreachable!(),
        };
        Ok(Some(CalleeBody {
            func: mf,
            canonical,
            subject: inst.clone(),
            abi,
            kind: Kind::Straight,
            ret_arity,
            reachable: Vec::new(),
            insts: reachable_insts,
            bounded: has_bounds,
            template,
            coalescable_params,
        }))
    } else {
        // General: uniform return arity across every reachable Return.
        let reachable: Vec<Idx<Block>> = source_reachable
            .iter()
            .map(|block| block_map[block])
            .collect();
        let mut arities = reachable.iter().filter_map(|&b| match &mf.blocks[b].term {
            Term::Return(vs) => Some(vs.len()),
            _ => None,
        });
        let Some(ret_arity) = arities.next() else {
            return reject_candidate(&mut timing, "no_return");
        };
        if !arities.all(|a| a == ret_arity) {
            return reject_candidate(&mut timing, "nonuniform_return_arity");
        }
        Ok(Some(CalleeBody {
            func: mf,
            canonical,
            subject: inst.clone(),
            abi,
            kind: Kind::General,
            ret_arity,
            reachable,
            insts: reachable_insts,
            bounded: has_bounds,
            template,
            coalescable_params,
        }))
    }
}

/// Get (lowering+classifying on first sight) the candidate for `inst`.
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
        let mut timing = CandidateTiming::default();
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
                if let Some(reason) = timing.rejection {
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

/// One resolved, eligible call site in the caller, extracted before the splice.
/// `inst` is the concrete body it targets, read from the caller's
/// `callee_instances`; a delivery record is only checked against it when one
/// exists. A missing generic owner leaves the call residual rather than guessing
/// a declaration-level body.
enum Site {
    /// `Inst::Call` — single (or void) result; `call_id` is the call's value.
    Single {
        bid: Idx<Block>,
        idx: usize,
        call_id: Idx<Inst>,
        args: Vec<Idx<Inst>>,
        inst: Instance,
        abi: BackendFnAbiPlanHandle,
    },
    /// `Inst::CallMulti` — N leaves bound positionally into `dests`.
    Multi {
        bid: Idx<Block>,
        idx: usize,
        call_id: Idx<Inst>,
        dests: Vec<Idx<Cell>>,
        args: Vec<Idx<Inst>>,
        inst: Instance,
        abi: BackendFnAbiPlanHandle,
    },
}

impl Site {
    fn inst(&self) -> &Instance {
        match self {
            Site::Single { inst, .. } | Site::Multi { inst, .. } => inst,
        }
    }
    fn fref(&self) -> FuncRef {
        self.inst().func
    }

    fn abi(&self) -> &BackendFnAbiPlan {
        match self {
            Site::Single { abi, .. } | Site::Multi { abi, .. } => abi,
        }
    }
    fn pos(&self) -> (Idx<Block>, usize) {
        match self {
            Site::Single { bid, idx, .. } | Site::Multi { bid, idx, .. } => (*bid, *idx),
        }
    }
    fn call_id(&self) -> Idx<Inst> {
        match self {
            Site::Single { call_id, .. } | Site::Multi { call_id, .. } => *call_id,
        }
    }
}

/// Resolve a lowered call's authoritative concrete owner. The delivery map is
/// only an agreement check: it is absent for valid residual concrete calls and
/// must never be used to invent the candidate identity.
fn call_target(
    func: &MirFunc,
    call_id: Idx<Inst>,
    fref: FuncRef,
    ws: &Workspace,
) -> Result<Option<(Instance, BackendFnAbiPlanHandle)>, CompilerError> {
    let Some(owner) = func.callee_instances.get(&call_id) else {
        return Ok(None);
    };
    if skull_mir::lower::canonical_variant(ws, owner.func)
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

/// Whether candidate `cand` matches the call site's arg/result arity.
fn arity_ok(cand: &CalleeBody, site: &Site) -> bool {
    match site {
        Site::Single { args, .. } => cand.func.params.len() == args.len() && cand.ret_arity <= 1,
        Site::Multi { args, dests, .. } => {
            cand.func.params.len() == args.len() && cand.ret_arity == dests.len()
        }
    }
}

/// Scan the caller for the next eligible call site (Call/CallMulti to a cached
/// candidate with matching arity, not the caller itself), starting AT `resume`.
///
/// `top` is the caller's own top-level blocks (NEVER a closure sub-CFG — a cell
/// used in a lambda body is an upvalue, so inlining there piles onto LuaJIT's
/// 60-upvalue cap, and the block-split assumes the top-level CFG). The driver
/// maintains it incrementally so this never recomputes reachability.
///
/// Blocks are visited in arena-id order (stable: a splice only appends blocks
/// with higher ids and only inserts insts AT/after its own position), so resuming
/// at the last splice point is sound — no eligible call can appear before it (the
/// prefix was already scanned and a splice introduces calls only within the
/// inlined body, at/after that point).
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
        // Find the inst index to start scanning this block from.
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
            let site = match &func.insts[iid] {
                Inst::Call { func: fref, args } if *fref != self_ref => {
                    scan_stats.call_instructions_examined += 1;
                    let Some((inst, abi)) = call_target(func, iid, *fref, ws)? else {
                        cache.rejection("missing_or_incomplete_concrete_owner");
                        continue;
                    };
                    scan_stats.call_target_resolutions += 1;
                    Site::Single {
                        bid,
                        idx,
                        call_id: iid,
                        args: args.clone(),
                        inst,
                        abi,
                    }
                }
                Inst::CallMulti {
                    dests,
                    func: fref,
                    args,
                } if *fref != self_ref => {
                    scan_stats.call_instructions_examined += 1;
                    let Some((inst, abi)) = call_target(func, iid, *fref, ws)? else {
                        cache.rejection("missing_or_incomplete_concrete_owner");
                        continue;
                    };
                    scan_stats.call_target_resolutions += 1;
                    Site::Multi {
                        bid,
                        idx,
                        call_id: iid,
                        dests: dests.clone(),
                        args: args.clone(),
                        inst,
                        abi,
                    }
                }
                _ => continue,
            };
            let candidate = get(cache, ws, scx, site.inst(), concrete_plans)?;
            let Some(cand) = candidate else {
                continue;
            };
            let reason = if cand.subject != *site.inst() {
                Some("candidate_subject_mismatch")
            } else if cand.abi.as_ref() != site.abi() {
                Some("candidate_abi_mismatch")
            } else if cand.abi.result_lanes() != site.abi().result_lanes() {
                Some("candidate_result_lanes_mismatch")
            } else if !arity_ok(cand, &site) {
                Some("candidate_arity_mismatch")
            } else if site.inst().args.is_empty()
                && cell_budget + cand.template.source_cells + cand.func.params.len() > CELL_BUDGET
                && cell_budget + cand.template.source_cells + cand.func.params.len()
                    - site_coalesced_cells(func, cand, &site, pinned)
                    > CELL_BUDGET
            {
                // Each argument is charged as if a caller local held it: one the
                // splice coalesces already sits in a counted caller cell, any
                // other costs one. `let x = e; f(x)` and `f(e)` then meet the
                // budget at the same size, so binding a local without a cell
                // cannot change what inlines.
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

/// How many of `cand`'s parameters the splice at `site` would coalesce.
fn site_coalesced_cells(
    func: &MirFunc,
    cand: &CalleeBody,
    site: &Site,
    pinned: &HashSet<Idx<Cell>>,
) -> usize {
    let (Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. }) = site;
    for_each_coalesced(func, cand, args, (*bid, *idx), pinned, |_, _| {})
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

/// Clone the callee's cells + insts into `caller` (NOT blocks — only the straight
/// path uses this; straight candidates are lambda-free so no inst references a
/// block). Returns `(inst_map, cell_map)`.
fn clone_cells_insts(
    caller: &mut MirFunc,
    callee: &MirFunc,
    scratch: &mut InlineScratch,
) -> CloneMapStats {
    let track_emit_ids = caller.emit_ids.is_some() || callee.emit_ids.is_some();
    if track_emit_ids {
        caller.ensure_emit_ids();
    }
    let (inst_base, cell_base, block_base) = caller.emit_nexts().unwrap_or_default();
    caller.insts.reserve(callee.insts.len());
    caller
        .cells
        .reserve(callee.cells.len().saturating_sub(scratch.overrides.len()));
    scratch.cell_map.clear();
    let cell_capacity = scratch.cell_map.capacity();
    scratch.cell_map.reserve(callee.cells.len());
    let mut skipped_cells = 0u32;
    for (cid, cell) in callee.cells.iter() {
        // A coalesced param maps to the caller cell its arg loads (no fresh copy).
        let mapped = if let Some(&mapped) = scratch.overrides.get(&cid) {
            skipped_cells += 1;
            mapped
        } else if track_emit_ids {
            let emit_id = cell_base + callee.emit_cell_id(cid) - skipped_cells;
            caller.alloc_cell_with_emit_id(cell.clone(), emit_id)
        } else {
            caller.cells.alloc(cell.clone())
        };
        scratch.cell_map.insert(cid, mapped);
    }
    mark_forwarded_params(caller, callee, scratch);
    scratch.inst_map.clear();
    let inst_capacity = scratch.inst_map.capacity();
    scratch.inst_map.reserve(callee.insts.len());
    scratch.inst_ids.clear();
    scratch.inst_ids.reserve(callee.insts.len());
    scratch.block_map.clear();
    for (iid, inst) in callee.insts.iter() {
        let nid = if track_emit_ids {
            caller.alloc_inst_with_emit_id(inst.clone(), inst_base + callee.emit_inst_id(iid))
        } else {
            caller.insts.alloc(inst.clone())
        };
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
    if track_emit_ids {
        caller.set_emit_nexts((
            inst_base + callee.emit_inst_span(),
            cell_base + callee.emit_cell_span() - skipped_cells,
            block_base,
        ));
    }

    CloneMapStats {
        inst_entries: scratch.inst_map.len(),
        cell_entries: scratch.cell_map.len(),
        inst_capacity: scratch.inst_map.capacity(),
        cell_capacity: scratch.cell_map.capacity(),
        inst_growth: usize::from(scratch.inst_map.capacity() != inst_capacity),
        cell_growth: usize::from(scratch.cell_map.capacity() != cell_capacity),
        ..CloneMapStats::default()
    }
}

/// Clone the callee's cells, blocks, AND insts into `caller`. Returns
/// `(inst_map, cell_map, block_map)`. Two-pass (alloc-then-remap) so it is
/// independent of arena order.
fn clone_all(caller: &mut MirFunc, callee: &MirFunc, scratch: &mut InlineScratch) -> CloneMapStats {
    let track_emit_ids = caller.emit_ids.is_some() || callee.emit_ids.is_some();
    if track_emit_ids {
        caller.ensure_emit_ids();
    }
    let (inst_base, cell_base, block_base) = caller.emit_nexts().unwrap_or_default();
    caller.insts.reserve(callee.insts.len());
    caller
        .cells
        .reserve(callee.cells.len().saturating_sub(scratch.overrides.len()));
    caller.blocks.reserve(callee.blocks.len());
    scratch.cell_map.clear();
    let cell_capacity = scratch.cell_map.capacity();
    scratch.cell_map.reserve(callee.cells.len());
    let mut skipped_cells = 0u32;
    for (cid, cell) in callee.cells.iter() {
        // A coalesced param maps to the caller cell its arg loads (no fresh copy).
        let mapped = if let Some(&mapped) = scratch.overrides.get(&cid) {
            skipped_cells += 1;
            mapped
        } else if track_emit_ids {
            let emit_id = cell_base + callee.emit_cell_id(cid) - skipped_cells;
            caller.alloc_cell_with_emit_id(cell.clone(), emit_id)
        } else {
            caller.cells.alloc(cell.clone())
        };
        scratch.cell_map.insert(cid, mapped);
    }
    mark_forwarded_params(caller, callee, scratch);
    import_cleanup_load_cells(caller, callee, &scratch.cell_map);
    if track_emit_ids {
        let forced = map_forced_cells(caller, callee, &scratch.overrides, cell_base, block_base);
        let ids = caller.emit_ids.as_mut().unwrap();
        for (cell, block) in forced {
            ids.forced_cells.push(cell);
            ids.forced_cell_blocks.push(block);
        }
    }
    let block_capacity = scratch.block_map.capacity();
    scratch.block_map.clear();
    scratch.block_map.reserve(callee.blocks.len());
    for (bid, _) in callee.blocks.iter() {
        let nbid = if track_emit_ids {
            caller.alloc_block_with_emit_id(block_base + callee.emit_block_id(bid))
        } else {
            caller.blocks.alloc(Block::new())
        };
        scratch.block_map.insert(bid, nbid);
    }
    scratch.inst_map.clear();
    let inst_capacity = scratch.inst_map.capacity();
    scratch.inst_map.reserve(callee.insts.len());
    scratch.inst_ids.clear();
    scratch.inst_ids.reserve(callee.insts.len());
    for (iid, inst) in callee.insts.iter() {
        let nid = if track_emit_ids {
            caller.alloc_inst_with_emit_id(inst.clone(), inst_base + callee.emit_inst_id(iid))
        } else {
            caller.insts.alloc(inst.clone())
        };
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
    // Fill cloned block inst-lists + terminators.
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
    if track_emit_ids {
        caller.set_emit_nexts((
            inst_base + callee.emit_inst_span(),
            cell_base + callee.emit_cell_span() - skipped_cells,
            block_base + callee.emit_block_span(),
        ));
    }

    CloneMapStats {
        inst_entries: scratch.inst_map.len(),
        cell_entries: scratch.cell_map.len(),
        block_entries: scratch.block_map.len(),
        inst_capacity: scratch.inst_map.capacity(),
        cell_capacity: scratch.cell_map.capacity(),
        block_capacity: scratch.block_map.capacity(),
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

/// Remap a freshly-cloned inst's value (via [`remap_inst`]), cell, and block
/// operands through the clone maps.
fn remap_clone(
    inst: &mut Inst,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    cm: &HashMap<Idx<Cell>, Idx<Cell>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    crate::remap_inst(inst, &|x| im[&x]);
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

/// Remap a cloned terminator's inst operands AND block targets.
fn remap_term_clone(
    term: &mut Term,
    im: &HashMap<Idx<Inst>, Idx<Inst>>,
    bm: &HashMap<Idx<Block>, Idx<Block>>,
) {
    let mi = |x: Idx<Inst>| im[&x];
    match term {
        Term::Return(vs) => vs.iter_mut().for_each(|v| *v = mi(*v)),
        Term::Goto(b) => *b = bm[b],
        Term::Branch {
            cond,
            then_bb,
            else_bb,
        } => {
            *cond = mi(*cond);
            *then_bb = bm[then_bb];
            *else_bb = bm[else_bb];
        }
        Term::Switch {
            discr,
            cases,
            default,
        } => {
            *discr = mi(*discr);
            for (_, b) in cases.iter_mut() {
                *b = bm[b];
            }
            *default = bm[default];
        }
        Term::Yield { values, resume } => {
            values.iter_mut().for_each(|v| *v = mi(*v));
            *resume = bm[resume];
        }
        Term::Unreachable => {}
    }
}

/// For each coalescable param whose arg is a plain `Load(c)` that still holds
/// when the body runs ([`snapshot_holds`]), map the callee param cell -> the
/// caller cell `c` (so the clone binds the param directly to it) and flag the
/// param so the splice skips its `Store(param, arg)` copy.
/// Removing that whole-cell copy is what keeps a by-ref-mutated cursor struct a
/// SINGLE cell, which SROA can then scalarize (the zero-alloc iterator path).
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

/// Visit each parameter `i` the splice at `pos` binds directly to caller cell
/// `c`; returns how many there are.
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

/// Whether cell `c` still holds the value `load` read when the call at
/// `bid[idx]` runs, and keeps it for the whole spliced body. The parameter is a
/// snapshot of its argument: coalescing it onto `c` is only sound when nothing
/// can write `c` from the read to the end of the body. The callee itself never
/// rebinds a coalescable parameter, so the writers left are caller code on a
/// path from the read to the call (a later argument, or a body spliced for an
/// earlier call) and closures the body can reach (`pinned`, see [`pin_cells`]).
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
    // The read is in another block. It dominates the call, so walking
    // predecessors back from the call without passing the read's block visits
    // exactly the blocks a path from the read to the call can cross; re-entering
    // the read's block would re-run the read, so only its tail counts.
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

/// Add the caller cells a spliced body could observe changing behind a
/// coalesced parameter: those written inside a closure body (any block outside
/// `top`), since the body could call that closure. A `ref` alias writes fields
/// of the object a cell holds, never the cell, so it does not pin. Only blocks at or past `blocks_from` are examined, so each splice
/// pins just what it imported.
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

/// After cloning a callee body into the caller: substitute every cloned
/// placeholder [`Inst::CallBoundMulti`] with the concrete `next` from the
/// site's tree, and import the callee's own carried `bound_trees` (keys
/// remapped, `CallerGeneric` leaves grafted through the site tree) so the
/// fixpoint can keep inlining the now-concrete calls.
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

/// Carry named nested bodies across a splice. Their names are private globals,
/// so each splice needs a fresh namespace and every reference in the cloned
/// parent and child arenas must move with it.
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

/// Import the cloned callee body's `spec_bindings` into the caller (keys remapped
/// through the clone) so a specialized call INSIDE a spliced body is itself found
/// and specialized by the fixpoint -- the `spec_bindings` analogue of
/// `resolve_cloned_bound_calls` importing `bound_trees`.
///
/// Resolved bound trees are instruction-keyed too. They carry concrete leaves
/// from a prior splice or standalone substitution, so dropping one would make
/// the next splice re-derive a foreign tree from the caller's inference arena.
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
    // The IDENTITY record travels with the body for the same reason the site
    // does: a spliced call still targets the instance the callee body named.
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
    // A minted fn-value symbol travels with the body that mints it: the callee's
    // `let f = adder` is still a reference to `adder` after the splice, and the
    // census joins it there. Dropping it would leave a spelled callee symbol in
    // the caller with no recorded declaration -- exactly the fail-open the
    // record exists to prevent.
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
    // The callee's instruction spans come across with its body, so a rejection
    // raised after inlining still points at the construct in the CALLEE's source
    // rather than at the call site that absorbed it.
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
    // Site identity travels with the body, and its OWNER travels with it: a
    // spliced call is the CALLEE instance's obligation, so the owner is
    // re-interned into the caller's table and the entry re-pointed. Copying the
    // `OwnerRef` verbatim would silently reinterpret the callee's index in the
    // caller's table -- index 0 there is the CALLER, so every spliced call
    // would join through the wrong body while looking perfectly well-formed.
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

/// Splice candidate `cand` into `func` at `site`, removing the call. Returns the
/// new top-level (reachable) blocks the splice added, for the driver to fold into
/// its top-level set (straight adds none).
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
    match cand.kind {
        Kind::Straight => {
            let (maps, coalesced) = splice_straight(
                func,
                site,
                cand,
                ws,
                scx,
                concrete_plans,
                pinned,
                scratch,
                uses,
                census,
            )?;
            Ok((maps, coalesced))
        }
        Kind::General => splice_general(
            func,
            site,
            cand,
            ws,
            scx,
            concrete_plans,
            pinned,
            scratch,
            uses,
            census,
        ),
    }
}

/// Straight splice: the callee is a single Return-terminated block. Inline its
/// insts into the caller block in place of the call; no new blocks (the callee
/// is lambda-free, so its insts reference no block). The caller block keeps its
/// own terminator.
fn splice_straight(
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
    let callee = &cand.func;
    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => {
            (*bid, *idx, args.clone())
        }
    };
    let site_tree = func.bound_trees.remove(&site.call_id());
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id());
    let coalesced_cells = coalesce_overrides(func, cand, &args, (bid, idx), pinned, scratch);
    let maps = clone_cells_insts(func, callee, scratch);
    // The bound element repack allocates after the clone, outside `inst_ids`.
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
    let nested_names = import_nested_fns(func, callee, &scratch.inst_map);
    import_spec_bindings(func, callee, &scratch.inst_map, &nested_names);
    for &iid in &scratch.inst_ids {
        uses.add_inst(func, iid);
    }
    uses.add_insts_from(func, repack_mark);

    // The call leaves the block, so its argument uses disappear. Its result
    // users remain indexed until the exact binding below replaces them.
    uses.remove_inst(func, site.call_id());

    // Rebuild the caller block: before | arg-stores | body | dest-stores | after.
    let old = std::mem::take(&mut func.blocks[bid].insts);
    let mut new_insts: Vec<Idx<Inst>> = old[..idx].to_vec();
    for (i, (param, arg)) in callee.params.iter().zip(args.iter()).enumerate() {
        if !scratch.coalesced[i] {
            let pc = scratch.cell_map[param];
            let store = func.alloc_inst(Inst::Store {
                cell: pc,
                value: *arg,
            });
            uses.add_inst(func, store);
            new_insts.push(store);
        }
    }
    new_insts.extend(
        callee.blocks[callee.entry]
            .insts
            .iter()
            .map(|inst| scratch.inst_map[inst]),
    );

    match &site {
        Site::Multi { dests, .. } => {
            let values = match &callee.blocks[callee.entry].term {
                Term::Return(values) => values,
                _ => unreachable!("straight candidate entry must end in Return"),
            };
            for (dest, rv) in dests.iter().zip(values.iter()) {
                let st = func.alloc_inst(Inst::Store {
                    cell: *dest,
                    value: scratch.inst_map[rv],
                });
                uses.add_inst(func, st);
                new_insts.push(st);
            }
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
        }
        Site::Single { call_id, .. } => {
            // Preserve the old straight-line MIR shape: the returned value
            // replaces the call at its exact indexed users, including uses in
            // successor blocks. No bridge cell or whole-caller scan is needed.
            let to = match &callee.blocks[callee.entry].term {
                Term::Return(values) => match values.first().map(|value| scratch.inst_map[value]) {
                    Some(value) => value,
                    None => {
                        func.insts[*call_id] = Inst::Const(Lit::Nil);
                        new_insts.push(*call_id);
                        *call_id
                    }
                },
                _ => unreachable!("straight candidate entry must end in Return"),
            };
            new_insts.extend_from_slice(&old[idx + 1..]);
            func.blocks[bid].insts = new_insts;
            let binding_started = census.enabled.then(Instant::now);
            if to != *call_id {
                let (instructions, terminators, replacements) = uses.replace(func, *call_id, to);
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
    Ok((maps, coalesced_cells))
}

/// General splice: the callee has internal control flow. Split the caller block
/// at the call; the prefix arg-stores + `Goto` the cloned entry; a continuation
/// block holds the post-call tail. Each cloned `Return` binds its values and
/// `Goto`s the continuation.
fn splice_general(
    func: &mut MirFunc,
    site: Site,
    cand: &CalleeBody,
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
    pinned: &HashSet<Idx<Cell>>,
    scratch: &mut InlineScratch,
    uses: &mut UseIndex,
    _census: &mut InlineScanStats,
) -> Result<(CloneMapStats, usize), CompilerError> {
    let callee = &cand.func;
    let (bid, idx, args) = match &site {
        Site::Single { bid, idx, args, .. } | Site::Multi { bid, idx, args, .. } => {
            (*bid, *idx, args.clone())
        }
    };
    let site_tree = func.bound_trees.remove(&site.call_id());
    let site_resolved_tree = func.resolved_bound_trees.remove(&site.call_id());
    let coalesced_cells = coalesce_overrides(func, cand, &args, (bid, idx), pinned, scratch);
    let maps = clone_all(func, callee, scratch);
    // The bound element repack allocates after the clone, outside `inst_ids`.
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
    let nested_names = import_nested_fns(func, callee, &scratch.inst_map);
    import_spec_bindings(func, callee, &scratch.inst_map, &nested_names);
    for &iid in &scratch.inst_ids {
        uses.add_inst(func, iid);
    }
    uses.add_insts_from(func, repack_mark);
    for &bid in scratch.block_map.values() {
        uses.add_term(func, bid);
    }

    // The call leaves the original block. Its argument uses disappear; the
    // original call value remains as the stable result-load identity below.
    uses.remove_inst(func, site.call_id());
    let entry_clone = scratch.block_map[&callee.entry];

    // Split the caller block at the call.
    let old = std::mem::take(&mut func.blocks[bid].insts);
    let before: Vec<Idx<Inst>> = old[..idx].to_vec();
    let after: Vec<Idx<Inst>> = old[idx + 1..].to_vec();
    let orig_term = func.blocks[bid].term.clone();
    uses.remove_term(func, bid);

    // Continuation block: post-call tail + the caller's original terminator.
    let cont = func.new_block();
    func.blocks[cont].insts = after;
    func.blocks[cont].term = orig_term;
    uses.add_term(func, cont);

    // Single-result: a result cell read back in the continuation. Repurpose the
    // call inst id in place as `Load(rc)` at the front of `cont` so downstream
    // refs (single-block, all in `cont`) resolve unchanged.
    let result_cell = match &site {
        Site::Single { call_id, .. } => {
            let rc = func.new_cell(None);
            func.cells[rc].producer = CellProducer::InlineResult;
            func.insts[*call_id] = Inst::Load(rc);
            func.blocks[cont].insts.insert(0, *call_id);
            Some(rc)
        }
        Site::Multi { .. } => None,
    };

    // Prefix: before | arg-stores; Goto the cloned entry. Coalesced params are
    // aliased to the caller cell (via `overrides`), so they need no copy.
    let mut prefix = before;
    for (i, (param, arg)) in callee.params.iter().zip(args.iter()).enumerate() {
        if scratch.coalesced[i] {
            continue;
        }
        let pc = scratch.cell_map[param];
        let store = func.alloc_inst(Inst::Store {
            cell: pc,
            value: *arg,
        });
        uses.add_inst(func, store);
        prefix.push(store);
    }
    func.blocks[bid].insts = prefix;
    func.blocks[bid].term = Term::Goto(entry_clone);

    // Route each cloned TOP-LEVEL Return to the continuation, binding return
    // values. Only the callee's reachable blocks: a lambda sub-CFG's Return is
    // the LAMBDA's return, not the callee's, and must stay put.
    for &source_block in &cand.reachable {
        let nb = scratch.block_map[&source_block];
        let vs = match &func.blocks[nb].term {
            Term::Return(vs) => vs.clone(),
            _ => continue,
        };
        uses.remove_term(func, nb);
        match &site {
            Site::Single { .. } => {
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
            Site::Multi { dests, .. } => {
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
    // New top-level blocks: the REACHABLE clones (cand.reachable mapped through
    // the clone) + the continuation. Dead cloned blocks are unreachable and must
    // NOT enter the top-level set (the old full re-scan never visited them).
    scratch
        .added_blocks
        .extend(cand.reachable.iter().map(|b| scratch.block_map[b]));
    scratch.added_blocks.push(cont);
    Ok((maps, coalesced_cells))
}

/// Inline eligible direct calls in `func` (an ordinary, non-coroutine caller).
/// Drives [`find_site`] -> [`splice`] to a fixpoint, bounded by the per-caller
/// budget. `self_ref` is the caller's own [`FuncRef`] (so a self-call is never
/// inlined). Run BEFORE `cleanup`.
///
/// Returns the set of callee [`FuncRef`]s spliced in, splice records, and the
/// instruction-scan census. The dependency set is the transitive closure of
/// bodies whose source now lives in `func`'s emitted chunk (the driver visits
/// each, including ones the inlined bodies themselves introduced). These are the
/// caller chunk's INLINE codegen dependencies: incremental emit must re-emit
/// `func` when any of them change.
pub fn inline_functions_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    func: &mut MirFunc,
    cache: &mut InlineCache,
    concrete_plans: &ConcretePlans,
) -> Result<(HashSet<FuncRef>, Vec<SpliceRecord>, InlineScanStats), CompilerError> {
    inline_functions_inner(ws, scx, self_ref, func, cache, concrete_plans)
}

fn inline_functions_inner(
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
    // Kill-switch for perf attribution / bisecting a suspected inline bug.
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
    let use_index_started = cache.census_enabled.then(Instant::now);
    let mut uses = UseIndex::build(func);
    if let Some(start) = use_index_started {
        cache.census.use_index_build_ns = start.elapsed().as_nanos() as u64;
        cache.census.use_index_values = uses.users.len();
    }
    let original_calls: HashSet<Idx<Inst>> = top
        .iter()
        .flat_map(|bid| func.blocks[*bid].insts.iter().copied())
        .filter(|iid| {
            matches!(
                &func.insts[*iid],
                Inst::Call { .. } | Inst::CallMulti { .. }
            )
        })
        .collect();
    // Resume cursor: the position of the last splice. No eligible call can appear
    // before it, so the next scan starts there (turns the per-splice full re-scan
    // from O(splices x size) into a single forward sweep).
    let mut resume: Option<(Idx<Block>, usize)> = None;
    let mut cell_budget = func.cells.len();
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
        let pos = site.pos();
        let fref = site.fref();
        if cache.census_enabled {
            scan_stats.spliced_keys.push(site.inst().clone());
        }
        let transitive = !original_calls.contains(&site.call_id());
        let argument_elements = match &site {
            Site::Single { args, .. } | Site::Multi { args, .. } => args.len(),
        };
        let destination_elements = match &site {
            Site::Single { .. } => 0,
            Site::Multi { dests, .. } => dests.len(),
        };
        let prefix = pos.1;
        let tail = func.blocks[pos.0].insts.len().saturating_sub(pos.1 + 1);
        // Re-borrow the candidate immutably for the splice (distinct from `func`).
        let (cands, census, scratch) = (&cache.cands, &mut cache.census, &mut cache.scratch);
        let cand = cands[site.inst()]
            .as_ref()
            .expect("site implies a cached candidate");
        // SAFETY-of-borrows: `splice` mutates `func`, reads `cand` (in `cache`);
        // they are different objects. The borrow checker can't see that through
        // the `&mut cache` in `find_site`, so split the lookup out here where
        // only `&cache` is held.
        let splice_start = cache.census_enabled.then(Instant::now);
        let arena_capacities = cache.census_enabled.then(|| {
            (
                func.insts.capacity(),
                func.cells.capacity(),
                func.blocks.capacity(),
            )
        });
        let result_cell =
            usize::from(cand.kind == Kind::General && matches!(&site, Site::Single { .. }));
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
            &mut uses,
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
        // The callee's `static` cells now live in THIS body: the spliced code
        // reads and writes them, and a `static` is one cell per owning fn, so
        // the cell is the same one every call site shares. Carrying them here is
        // the only way a cell whose owner has no standalone emission -- every
        // `Iterator`-bounded fn -- reaches the emitted program's registry.
        // Deduped below; one callee can be spliced at many sites.
        func.statics.extend(cand.func.statics.iter().cloned());
        func.comptime_reads
            .extend(cand.func.comptime_reads.iter().cloned());
        func.struct_registry_roots
            .extend(cand.func.struct_registry_roots.iter().copied());
        cell_budget += cand.template.source_cells.saturating_sub(coalesced_cells) + result_cell;
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
    uses.record_drift(func);
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

/// Whether the fn declares any `Iterator` bound on its generics -- such a fn has
/// no standalone emission (it exists only inlined at its call sites).
pub fn fn_has_bounds(f: &bone_core::hir::Function) -> bool {
    f.generic_bounds.iter().any(Option::is_some)
}

/// Post-inline gate: a body about to be emitted must contain no placeholder
/// bound-call, and no residual call to a bounded fn that NAMES NOTHING.
///
/// A residual call is fine when the lowerer gave it a specialization: it spells
/// the specialized symbol (`callee_name` / `spec_fn_name`), and that definition
/// is emitted standalone from the demand this same call site raises, with its
/// placeholders resolved by the demand's tree
/// (`skull_transform::bound::resolve_standalone_bound_calls`). What is still a
/// rejection is a residual call the inliner did not splice AND the lowerer did
/// not specialize -- there is then no body anywhere that serves it.
///
/// Shared by the luajit driver (`skull::mir_emit`) and the JS closure emitter.
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

    // The `loop_binding` provenance tag lives ON the cell (unfuck #37), so a
    // transform that clones cells carries it for free -- no side-table to remap.
    // This is what will keep a spliced loop counter per-iteration once the
    // lambda-callee refusal (`classify`, `Inst::Lambda -> None`) is lifted.
    #[test]
    fn loop_binding_tag_survives_cell_clone_through_splice() {
        let mut callee = MirFunc::new(Ustr::from("callee"));
        let counter = callee.new_cell(Some(Ustr::from("i")));
        callee.mark_loop_binding(counter);
        let plain = callee.new_cell(Some(Ustr::from("x")));
        callee.insts.alloc(Inst::Const(Lit::Nil));

        let mut caller = MirFunc::new(Ustr::from("caller"));
        let mut scratch = InlineScratch::default();
        let first = clone_cells_insts(&mut caller, &callee, &mut scratch);
        let second = clone_cells_insts(&mut caller, &callee, &mut scratch);

        assert!(first.inst_growth > 0 && second.inst_growth == 0);
        assert!(first.cell_growth > 0 && second.cell_growth == 0);

        assert!(
            caller.cells[scratch.cell_map[&counter]].loop_binding,
            "cloned loop-binding cell must keep its tag"
        );
        assert!(
            !caller.cells[scratch.cell_map[&plain]].loop_binding,
            "an ordinary cloned cell must not gain the tag"
        );
    }

    /// A caller `x = 1; arg = Load(x); <between>; call(arg)`, returning the
    /// function, `x`, the argument load and the call position.
    fn snapshot_caller(
        between: impl FnOnce(&mut MirFunc, Idx<Block>, Idx<Cell>) -> Idx<Block>,
    ) -> (MirFunc, Idx<Cell>, Idx<Inst>, (Idx<Block>, usize)) {
        let mut func = MirFunc::new(Ustr::from("caller"));
        let entry = func.entry;
        let x = func.new_cell(Some(Ustr::from("x")));
        let one = func.emit(entry, Inst::Const(Lit::Int(1)));
        func.emit(
            entry,
            Inst::Store {
                cell: x,
                value: one,
            },
        );
        let load = func.emit(entry, Inst::Load(x));
        let call_block = between(&mut func, entry, x);
        let idx = func.blocks[call_block].insts.len();
        func.emit(call_block, Inst::Const(Lit::Nil));
        func.set_term(call_block, Term::Return(vec![]));
        (func, x, load, (call_block, idx))
    }

    fn holds(func: &MirFunc, x: Idx<Cell>, load: Idx<Inst>, pos: (Idx<Block>, usize)) -> bool {
        let top: HashSet<Idx<Block>> = top_level_blocks(func).into_iter().collect();
        let mut pinned = HashSet::default();
        pin_cells(func, &top, 0, &mut pinned);
        snapshot_holds(func, &pinned, &mut None, x, load, pos)
    }

    #[test]
    fn coalescing_requires_the_argument_cell_unwritten_until_the_call() {
        let (func, x, load, pos) = snapshot_caller(|_, entry, _| entry);
        assert!(holds(&func, x, load, pos), "nothing writes x");

        let (func, x, load, pos) = snapshot_caller(|func, entry, x| {
            let five = func.emit(entry, Inst::Const(Lit::Int(5)));
            func.emit(
                entry,
                Inst::Store {
                    cell: x,
                    value: five,
                },
            );
            entry
        });
        assert!(!holds(&func, x, load, pos), "a later argument writes x");

        let (func, x, load, pos) = snapshot_caller(|func, entry, _| {
            let next = func.new_block();
            func.set_term(entry, Term::Goto(next));
            next
        });
        assert!(holds(&func, x, load, pos), "a clean block path keeps x");

        let (func, x, load, pos) = snapshot_caller(|func, entry, x| {
            let (left, right, join) = (func.new_block(), func.new_block(), func.new_block());
            let flag = func.emit(entry, Inst::Const(Lit::Bool(true)));
            func.set_term(
                entry,
                Term::Branch {
                    cond: flag,
                    then_bb: left,
                    else_bb: right,
                },
            );
            let nine = func.emit(right, Inst::Const(Lit::Int(9)));
            func.emit(
                right,
                Inst::Store {
                    cell: x,
                    value: nine,
                },
            );
            func.set_term(left, Term::Goto(join));
            func.set_term(right, Term::Goto(join));
            join
        });
        assert!(!holds(&func, x, load, pos), "one branch writes x");
    }

    #[test]
    fn coalescing_refuses_a_cell_a_closure_body_writes() {
        let (func, x, load, pos) = snapshot_caller(|func, entry, x| {
            let body = func.new_block();
            let seven = func.emit(body, Inst::Const(Lit::Int(7)));
            func.emit(
                body,
                Inst::Store {
                    cell: x,
                    value: seven,
                },
            );
            func.set_term(body, Term::Return(vec![]));
            func.emit(
                entry,
                Inst::Lambda {
                    params: vec![],
                    entry: body,
                },
            );
            entry
        });
        assert!(
            !holds(&func, x, load, pos),
            "the spliced body could call the closure"
        );
    }

    #[test]
    fn compact_template_excludes_dead_arena_entries() {
        let mut source = MirFunc::new(Ustr::from("template"));
        let live = source.emit(source.entry, Inst::Const(Lit::Nil));
        source.set_term(source.entry, Term::Return(vec![live]));
        source.insts.alloc(Inst::Const(Lit::Bool(true)));
        source.new_cell(Some(Ustr::from("dead")));
        source.new_block();

        let live = live_closure(&source);
        let counts = template_counts(&source, &live);
        let (template, _) = compact_live_template(source, &live);

        assert_eq!(
            (counts.source_instructions, counts.live_instructions),
            (2, 1)
        );
        assert_eq!((counts.source_cells, counts.live_cells), (1, 0));
        assert_eq!((counts.source_blocks, counts.live_blocks), (2, 1));
        assert_eq!(template.insts.len(), counts.live_instructions);
        assert_eq!(template.cells.len(), counts.live_cells);
        assert_eq!(template.blocks.len(), counts.live_blocks);
    }

    #[test]
    fn compact_template_preserves_source_emission_ids() {
        let mut source = MirFunc::new(Ustr::from("template_ids"));
        let dead = source.new_cell(Some(Ustr::from("dead")));
        let live = source.new_cell(Some(Ustr::from("live")));
        let value = source.emit(source.entry, Inst::Load(live));
        source.set_term(source.entry, Term::Return(vec![value]));
        let dead_block = source.new_block();
        let dead_value = source.emit(dead_block, Inst::Load(dead));
        source.set_term(dead_block, Term::Return(vec![dead_value]));

        let live = live_closure(&source);
        let (template, _) = compact_live_template(source, &live);

        assert_eq!(template.cells.len(), 1);
        assert_eq!(
            template.emit_cell_id(template.cells.iter().next().unwrap().0),
            1
        );
        assert_eq!(template.emit_cell_span(), 2);
        assert_eq!(template.blocks.len(), 1);
        assert_eq!(template.emit_block_span(), 2);
    }

    #[test]
    fn compact_template_remaps_parameter_default_guards() {
        let mut source = MirFunc::new(Ustr::from("template_default_guard"));
        let dead = source.new_block();
        source.emit(dead, Inst::Const(Lit::Bool(false)));
        source.set_term(dead, Term::Return(Vec::new()));
        let guard = source.emit(source.entry, Inst::Const(Lit::Bool(true)));
        source.param_default_guards.insert(guard, 2);
        source.set_term(source.entry, Term::Return(vec![guard]));

        let live = live_closure(&source);
        let (template, _) = compact_live_template(source, &live);

        let remapped_guard = template.blocks[template.entry].insts[0];
        assert_eq!(template.param_default_guards.get(&remapped_guard), Some(&2));
    }

    #[test]
    fn compact_template_records_dead_block_cleanup_loads() {
        let mut source = MirFunc::new(Ustr::from("template_cleanup_roots"));
        let cell = source.new_cell(Some(Ustr::from("value")));
        let value = source.emit(source.entry, Inst::Const(Lit::Int(1)));
        source.emit(source.entry, Inst::Store { cell, value });
        source.set_term(source.entry, Term::Return(Vec::new()));
        let dead = source.new_block();
        let loaded = source.emit(dead, Inst::Load(cell));
        source.set_term(dead, Term::Return(vec![loaded]));

        let live = live_closure(&source);
        let (template, _) = compact_live_template(source, &live);

        let expected = template.cells.iter().next().unwrap().0;
        assert_eq!(template.inline_cleanup_load_cells, vec![expected]);
    }

    #[test]
    fn folded_dead_blocks_drop_virtual_forced_cell_provenance() {
        let mut func = MirFunc::new(Ustr::from("forced_provenance"));
        let cond = func.emit(func.entry, Inst::Const(Lit::Bool(true)));
        let live = func.new_block();
        let dead = func.new_block();
        let cell = func.new_cell(None);
        let value = func.emit(dead, Inst::Load(cell));
        func.set_term(
            func.entry,
            Term::Branch {
                cond,
                then_bb: live,
                else_bb: dead,
            },
        );
        func.set_term(live, Term::Return(Vec::new()));
        func.set_term(dead, Term::Return(vec![value]));
        func.ensure_emit_ids();
        let dead_cell_id = func.emit_cell_id(cell);
        let dead_block_id = func.emit_block_id(dead);
        func.emit_ids
            .as_mut()
            .unwrap()
            .forced_cells
            .push(dead_cell_id);
        func.emit_ids
            .as_mut()
            .unwrap()
            .forced_cell_blocks
            .push(dead_block_id);

        assert!(crate::fold_const_branches(&mut func));
        assert!(func.emit_ids.as_ref().unwrap().forced_cells.is_empty());
        assert!(
            func.emit_ids
                .as_ref()
                .unwrap()
                .forced_cell_blocks
                .is_empty()
        );
    }

    #[test]
    fn use_index_replaces_cross_block_users_once() {
        let mut func = MirFunc::new(Ustr::from("uses"));
        let from = func.emit(func.entry, Inst::Const(Lit::Int(1)));
        let user = func.emit(func.entry, Inst::IntAdd(from, from));
        let then_bb = func.new_block();
        let else_bb = func.new_block();
        func.set_term(
            func.entry,
            Term::Branch {
                cond: from,
                then_bb,
                else_bb,
            },
        );
        func.set_term(then_bb, Term::Return(vec![user]));
        func.set_term(else_bb, Term::Return(vec![from]));
        let to = func.emit(func.entry, Inst::Const(Lit::Int(2)));

        let mut uses = UseIndex::build(&func);
        let counts = uses.replace(&mut func, from, to);

        assert_eq!(counts, (1, 2, 4));
        assert_eq!(func.insts[user], Inst::IntAdd(to, to));
        assert_eq!(
            func.blocks[func.entry].term,
            Term::Branch {
                cond: to,
                then_bb,
                else_bb
            }
        );
        assert_eq!(func.blocks[else_bb].term, Term::Return(vec![to]));
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
            .find(|(_, f)| f.name.as_str() == "top")
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
            ctor: skull_mir::monoty::MonoCtor::Nominal(Ustr::from("Pair")),
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
            let sites: Vec<_> = plan
                .callees
                .iter()
                .filter(|(_, callee)| **callee == witness)
                .map(|(site, _)| site.clone())
                .collect();
            for site in sites {
                let signature = plan
                    .callee_signatures
                    .get_mut(&site)
                    .expect("witness site signature");
                std::sync::Arc::make_mut(signature).result = Some(wrong_result);
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

    #[test]
    fn annotation_gate_requires_explicit_opt_in() {
        assert!(!annotation_gate(false, false));
        assert!(annotation_gate(true, false));
        assert!(annotation_gate(false, true));
        assert!(annotation_gate(true, true));
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
            lower_ns: 0,
            full_clone_ns: 0,
            cleanup_ns: 0,
            compact_ns: 0,
            lower_attempted: false,
            lowered: false,
            raw_instructions: 0,
            raw_cells: 0,
            raw_blocks: 0,
            cleaned_instructions: 0,
            cleaned_cells: 0,
            cleaned_blocks: 0,
            full_clone_inst_capacity: 0,
            full_clone_cell_capacity: 0,
            full_clone_block_capacity: 0,
            compact_inst_capacity: 0,
            compact_cell_capacity: 0,
            compact_block_capacity: 0,
            eligibility_ns: 0,
            rejection,
            worker: "candidate-table-test".to_string(),
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

    fn resolved_tree_count(mf: &MirFunc) -> (usize, usize) {
        let mut bodies = 1;
        let mut trees = mf.resolved_bound_trees.len();
        for nf in &mf.nested_fns {
            let (child_bodies, child_trees) = resolved_tree_count(nf);
            bodies += child_bodies;
            trees += child_trees;
        }
        (bodies, trees)
    }

    #[test]
    fn fresh_candidates_have_no_resolved_bound_trees_including_nested_bodies() {
        let mut modules = vec![
            skull_parser::parse_module_clean(
                r#"
@inline
fn outer(x: int) -> int {
    fn inner(y: int) -> int { return y }
    return inner(x)
}
"#,
                Ustr::from("<inline-test>"),
            )
            .expect("parse"),
        ];
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic");
        let mid = *ws
            .module_ids
            .get(&Ustr::from("<inline-test>"))
            .expect("module");
        let (fid, _) = ws
            .module(mid)
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == "outer")
            .expect("outer");
        let root = FuncRef {
            module: mid,
            func: fid,
        };
        let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
        let graph = skull_graph::instance_graph::build_in(
            &ws,
            vec![root],
            false,
            std::sync::Arc::clone(&interner),
        );
        let concrete = skull_graph::concrete_body::build(&ws, &graph);
        let scx = ScalarCtx::new(&ws).sharing(interner);
        let plan = concrete
            .plans
            .get(&Instance::base(root))
            .expect("inline fixture concrete plan");
        let body = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
            .expect("the inline fixture must lower");
        let (bodies, trees) = resolved_tree_count(&body);
        assert!(
            bodies > 1,
            "the candidate census did not include its nested body"
        );
        assert_eq!(
            trees, 0,
            "fresh candidate lowering already carried resolved trees"
        );
    }
}
