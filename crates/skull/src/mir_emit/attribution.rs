//! Emit attribution and setup-lane measurement, reported by `rock backend-stats`.

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupLaneAttribution {
    pub cpu_population: usize,
    pub shader_population: usize,
    pub abi_population: usize,
    pub cpu_shader_overlap: bool,
    pub abi_cpu_overlap: bool,
    pub abi_shader_overlap: bool,
    pub first_started_lane: Option<SetupLaneKind>,
    pub mono_write_acqs_during_join: u64,
    pub mono_intern_attempts_during_join: u64,
    pub mono_intern_new_during_join: u64,
    #[cfg(test)]
    pub setup_handshake_entered: usize,
    #[cfg(test)]
    pub setup_handshake_released: usize,
    #[cfg(test)]
    pub setup_handshake_released_after_both: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupLaneKind {
    CpuArtifact,
    Shader,
    AbiWarm,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SetupLaneWindow {
    pub(super) started: Instant,
    pub(super) finished: Instant,
    pub(super) population: usize,
}

pub(super) fn setup_lanes_overlap(left: SetupLaneWindow, right: SetupLaneWindow) -> bool {
    left.started < right.finished && right.started < left.finished
}

pub(super) fn first_setup_lane(
    cpu: SetupLaneWindow,
    shader: SetupLaneWindow,
    abi: SetupLaneWindow,
) -> SetupLaneKind {
    let mut first = (cpu.started, SetupLaneKind::CpuArtifact);
    if shader.started < first.0 {
        first = (shader.started, SetupLaneKind::Shader);
    }
    if abi.started < first.0 {
        first = (abi.started, SetupLaneKind::AbiWarm);
    }
    first.1
}

#[derive(Clone, Debug, Default)]
pub struct EmitAttribution {
    pub bodies_examined: usize,
    pub bodies_with_work: usize,
    pub lower: AttributionPhase,
    pub inline_phase: AttributionPhase,
    pub inline: InlineAttribution,
    pub cleanup: AttributionPhase,
    pub closure: AttributionPhase,
    pub counted_loop: AttributionPhase,
    pub hoisted_inline: AttributionPhase,
    pub validation: AttributionPhase,
    pub regalloc: AttributionPhase,
    pub stringify: AttributionPhase,
    pub top_bodies: Vec<BodyAttributionOwner>,
}

#[derive(Clone, Debug, Default)]
pub struct AttributionPhase {
    pub bodies_examined: usize,
    pub bodies_with_work: usize,
    pub elapsed: Duration,
    pub instructions_visited: usize,
    pub work_volume: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub concrete_mono_requests: usize,
    pub concrete_mono_cache_hits: usize,
    pub concrete_mono_derivations: usize,
    pub concrete_mono_unique_ids: usize,
    pub concrete_mono_cloned_nodes: usize,
    pub concrete_mono_interner_miss_cloned_nodes: usize,
    pub concrete_mono_cache_insertion_cloned_nodes: usize,
    pub concrete_mono_cache_hit_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escape_cloned_nodes: usize,
    pub concrete_mono_explicit_owned_escapes: usize,
    pub concrete_mono_lookup_ns: u64,
    pub concrete_mono_cache_lookup_ns: u64,
    pub concrete_mono_shared_handle_requests: usize,
    pub concrete_mono_clone_self_ns: u64,
    pub concrete_mono_timed_requests: usize,
    pub concrete_mono_by_consumer: [skull_mir::lower::ConcreteMonoConsumerStats;
        skull_mir::lower::ConcreteMonoConsumer::COUNT],
    pub abi_param_copy_volume: usize,
    pub abi_param_shared_views: usize,
    pub mir_instruction_allocations: usize,
    pub mir_instruction_reallocations: usize,
    pub mir_instruction_reallocation_ns: u64,
    pub mir_instruction_capacity: usize,
    pub mir_cell_allocations: usize,
    pub mir_cell_reallocations: usize,
    pub mir_cell_reallocation_ns: u64,
    pub mir_cell_capacity: usize,
    pub mir_block_allocations: usize,
    pub mir_block_reallocations: usize,
    pub mir_block_reallocation_ns: u64,
    pub mir_block_capacity: usize,
}

#[derive(Clone, Debug, Default)]
pub struct InlinePopulationAttribution {
    pub candidate_published_keys: usize,
    pub candidate_lower_attempted: usize,
    pub candidate_lowered_keys: usize,
    pub candidate_lower_failed: usize,
    pub candidate_eligible_keys: usize,
    pub candidate_rejected_before_lower: usize,
    pub candidate_rejected_after_lower: usize,
    pub candidate_full_clone_ns: u64,
    pub candidate_compact_ns: u64,
    pub candidate_raw_instructions: usize,
    pub candidate_raw_cells: usize,
    pub candidate_raw_blocks: usize,
    pub candidate_cleaned_instructions: usize,
    pub candidate_cleaned_cells: usize,
    pub candidate_cleaned_blocks: usize,
    pub candidate_full_clone_capacity: (usize, usize, usize),
    pub candidate_compact_capacity: (usize, usize, usize),
    pub ordinary_lowered_keys: usize,
    pub specialized_lowered_keys: usize,
    pub ordinary_lowering_derivations: usize,
    pub specialized_lowering_derivations: usize,
    pub ordinary_consumer_keys: usize,
    pub specialized_consumer_keys: usize,
    pub candidate_ordinary_consumer_overlap: usize,
    pub candidate_specialized_consumer_overlap: usize,
    pub ordinary_canonical_reuse_keys: usize,
    pub specialized_canonical_reuse_keys: usize,
    pub ordinary_canonical_miss_keys: usize,
    pub specialized_canonical_miss_keys: usize,
    pub ordinary_canonical_miss_instances: Vec<Instance>,
    pub specialized_canonical_miss_instances: Vec<Instance>,
    pub canonical_clone_derivations: usize,
    pub always_lowered_units: usize,
    pub always_callee_keys: usize,
    pub candidate_ordinary_overlap: usize,
    pub candidate_specialized_overlap: usize,
    pub candidate_always_callee_overlap: usize,
    pub ordinary_specialized_overlap: usize,
    pub candidate_requested_unique_keys: usize,
    pub candidate_spliced_unique_keys: usize,
    pub candidate_published_never_requested: usize,
    pub candidate_requested_never_spliced: usize,
    pub candidate_requested_not_published: usize,
    pub keys_lowered_more_than_once: usize,
    pub first_lower_ns: u64,
    pub duplicate_lower_ns: u64,
}

#[derive(Clone, Debug, Default)]
pub struct InlineAttribution {
    pub accepted_splices: usize,
    pub copied_instructions: usize,
    pub ordinary_splices: usize,
    pub specialized_splices: usize,
    pub bounded_splices: usize,
    pub hoisted_splices: usize,
    pub arena_allocations: usize,
    pub cleanup_live_before: usize,
    pub cleanup_live_after: usize,
    pub borrowed_block_scans: usize,
    pub instructions_scanned: usize,
    pub census: skull_transform::InlineScanStats,
    pub population: InlinePopulationAttribution,
    pub candidate_unique_keys: usize,
    pub candidate_snapshot_duplicates: usize,
    pub candidate_setup_keys: usize,
    pub candidate_setup_warmed: usize,
    pub candidate_setup_reused: usize,
    pub candidate_setup_eligible: usize,
    pub candidate_setup_rejected: usize,
    pub candidate_setup_lower_ns: u64,
    pub candidate_setup_cleanup_ns: u64,
    pub candidate_setup_eligibility_ns: u64,
    pub candidate_setup_worker_derivations: BTreeMap<String, usize>,
    pub candidate_setup_rejection_reasons: BTreeMap<String, usize>,
    pub top_owners: Vec<InlineOwnerAttribution>,
}

#[derive(Clone, Debug, Default)]
pub struct InlineOwnerAttribution {
    pub owner: String,
    pub splices: usize,
    pub copied_instructions: usize,
    pub splice_ns: u64,
    pub live_cloned_entries: usize,
    pub dead_cloned_entries: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BodyAttributionOwner {
    pub owner: String,
    pub lower_us: u64,
    pub mir_instructions: usize,
    pub concrete_mono_requests: usize,
    pub concrete_mono_cloned_nodes: usize,
}

/// Counter totals of the build's shared `RepArena` and `MonoInterner`, taken
/// when the body-lowering fanout began (`setup`) and again at emission end
/// (`final`). Surfaced by `backend-stats`; the freeze-premise gate for
/// publishing lock-free read views is `intern_new`/`write_acqs` deltas near
/// zero after setup.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaTraffic {
    pub rep_setup: skull_mir::lower::RepArenaStats,
    pub rep_final: skull_mir::lower::RepArenaStats,
    pub mono_setup: skull_mir::monoty::MonoInternerStats,
    pub mono_final: skull_mir::monoty::MonoInternerStats,
}

/// Coarse per-stage wall-clock for one emitted definition. Summed across every
/// cached chunk + decorator/global unit in [`assemble`] to produce the
/// `backend-stats` breakdown.
#[derive(Default, Clone, Copy)]
pub(crate) struct PhaseTimes {
    pub(super) lower: Duration,
    pub(super) lower_work: bool,
    pub(super) inline: Duration,
    pub(super) inline_work: bool,
    pub(super) cleanup: Duration,
    pub(super) cleanup_work: bool,
    pub(super) closure: Duration,
    pub(super) closure_work: bool,
    pub(super) counted_loop: Duration,
    pub(super) counted_loop_work: bool,
    pub(super) hoisted_inline: Duration,
    pub(super) hoisted_inline_work: bool,
    pub(super) validation: Duration,
    pub(super) validation_work: bool,
    pub(super) emit: Duration,
    pub(super) emit_work: bool,
    /// Regalloc subset of `emit` (read from the per-thread `REGALLOC_TIME`
    /// accumulator, reset just before this unit's emit so it is exact per-unit
    /// even when units run concurrently on a rayon worker pool).
    pub(super) regalloc: Duration,
    pub(super) regalloc_work: bool,
}

impl PhaseTimes {
    pub(super) fn add(&mut self, o: &PhaseTimes) {
        self.lower += o.lower;
        self.lower_work |= o.lower_work;
        self.inline += o.inline;
        self.inline_work |= o.inline_work;
        self.cleanup += o.cleanup;
        self.cleanup_work |= o.cleanup_work;
        self.closure += o.closure;
        self.closure_work |= o.closure_work;
        self.counted_loop += o.counted_loop;
        self.counted_loop_work |= o.counted_loop_work;
        self.hoisted_inline += o.hoisted_inline;
        self.hoisted_inline_work |= o.hoisted_inline_work;
        self.validation += o.validation;
        self.validation_work |= o.validation_work;
        self.emit += o.emit;
        self.emit_work |= o.emit_work;
        self.regalloc += o.regalloc;
        self.regalloc_work |= o.regalloc_work;
    }
}

#[derive(Default, Clone)]
pub(super) struct InlineWork {
    pub(super) accepted_splices: usize,
    copied_instructions: usize,
    pub(super) ordinary_splices: usize,
    pub(super) specialized_splices: usize,
    bounded_splices: usize,
    pub(super) hoisted_splices: usize,
    arena_allocations: usize,
    pub(super) cleanup_live_before: usize,
    pub(super) cleanup_live_after: usize,
    borrowed_block_scans: usize,
    instructions_scanned: usize,
    pub(super) census: skull_transform::InlineScanStats,
    owners: BTreeMap<String, (usize, usize, u64, usize, usize)>,
}

impl InlineWork {
    pub(super) fn add_records(&mut self, records: &[skull_transform::SpliceRecord]) {
        self.accepted_splices += records.len();
        for record in records {
            self.copied_instructions += record.callee_insts;
            self.bounded_splices += usize::from(record.bounded);
            self.census.clone.add(record.clone);
            let owner = inline_owner_key(record.callee);
            let entry = self.owners.entry(owner).or_default();
            entry.0 += 1;
            entry.1 += record.callee_insts;
            entry.2 += record.splice_ns;
            entry.3 +=
                record.clone.live_instructions + record.clone.live_cells + record.clone.live_blocks;
            entry.4 += record.clone.dead_cloned_instructions
                + record.clone.dead_cloned_cells
                + record.clone.dead_cloned_blocks;
        }
    }

    pub(super) fn add_allocations(
        &mut self,
        before: (usize, usize, usize),
        after: (usize, usize, usize),
    ) {
        self.arena_allocations += after.0.saturating_sub(before.0);
        self.arena_allocations += after.1.saturating_sub(before.1);
        self.arena_allocations += after.2.saturating_sub(before.2);
    }

    pub(super) fn add_scan(&mut self, stats: skull_transform::InlineScanStats) {
        self.borrowed_block_scans += stats.borrowed_block_scans;
        self.instructions_scanned += stats.instructions_scanned;
        self.census.add(stats);
    }

    pub(super) fn add(&mut self, other: InlineWork) {
        self.accepted_splices += other.accepted_splices;
        self.copied_instructions += other.copied_instructions;
        self.ordinary_splices += other.ordinary_splices;
        self.specialized_splices += other.specialized_splices;
        self.bounded_splices += other.bounded_splices;
        self.hoisted_splices += other.hoisted_splices;
        self.arena_allocations += other.arena_allocations;
        self.cleanup_live_before += other.cleanup_live_before;
        self.cleanup_live_after += other.cleanup_live_after;
        self.borrowed_block_scans += other.borrowed_block_scans;
        self.instructions_scanned += other.instructions_scanned;
        self.census.add(other.census);
        for (owner, (splices, copied, splice_ns, live, dead)) in other.owners {
            let entry = self.owners.entry(owner).or_default();
            entry.0 += splices;
            entry.1 += copied;
            entry.2 += splice_ns;
            entry.3 += live;
            entry.4 += dead;
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct UnitAttribution {
    pub(super) owner: String,
    pub(super) examined: bool,
    pub(super) times: PhaseTimes,
    pub(super) hir_nodes: usize,
    pub(super) mir_instructions: usize,
    pub(super) lower_facts: skull_mir::lower::LowerAttribution,
    pub(super) inline: InlineWork,
    pub(super) lowered_instance: Option<(Instance, LoweredInstanceKind)>,
    pub(super) consumer_instance: Option<(Instance, LoweredInstanceKind)>,
    pub(super) canonical_reused: bool,
    pub(super) always_lowered: bool,
    pub(super) always_callee_keys: Vec<Instance>,
}

#[derive(Clone, Copy)]
pub(super) enum LoweredInstanceKind {
    Ordinary,
    Specialized,
}

#[derive(Default)]
pub(super) struct AttributionAccumulator {
    pub(super) result: EmitAttribution,
    pub(super) lower_samples: Vec<u64>,
    pub(super) inline_samples: Vec<u64>,
    pub(super) cleanup_samples: Vec<u64>,
    pub(super) closure_samples: Vec<u64>,
    pub(super) counted_loop_samples: Vec<u64>,
    pub(super) hoisted_inline_samples: Vec<u64>,
    pub(super) validation_samples: Vec<u64>,
    pub(super) regalloc_samples: Vec<u64>,
    pub(super) stringify_samples: Vec<u64>,
    pub(super) top_bodies: Vec<BodyAttributionOwner>,
    pub(super) census_enabled: bool,
    pub(super) candidate_published_keys: FxHashSet<Instance>,
    pub(super) candidate_eligible_keys: FxHashSet<Instance>,
    pub(super) ordinary_lowered_keys: FxHashSet<Instance>,
    pub(super) specialized_lowered_keys: FxHashSet<Instance>,
    pub(super) ordinary_consumer_keys: FxHashSet<Instance>,
    pub(super) specialized_consumer_keys: FxHashSet<Instance>,
    pub(super) ordinary_canonical_reuse_keys: FxHashSet<Instance>,
    pub(super) specialized_canonical_reuse_keys: FxHashSet<Instance>,
    pub(super) canonical_clone_derivations: usize,
    pub(super) candidate_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    pub(super) ordinary_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    pub(super) specialized_lower_ns_by_key: BTreeMap<Instance, Vec<u64>>,
    pub(super) always_callee_keys: FxHashSet<Instance>,
    pub(super) always_lowered_units: usize,
}

pub(super) fn inline_owner_key(fref: FuncRef) -> String {
    format!(
        "m{}:f{}",
        fref.module.index(),
        u32::from(fref.func.into_raw())
    )
}

pub(super) fn hir_nodes_for_owner(ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> usize {
    ws.module(mid)
        .bodies
        .get(&owner)
        .map(|body| {
            body.expr_arena.len()
                + body.binding_arena.len()
                + body.statement_arena.len()
                + body.pattern_arena.len()
                + body.match_arm_arena.len()
        })
        .unwrap_or(0)
}

pub(super) fn mir_live_instructions(mf: &MirFunc) -> usize {
    mf.blocks.iter().map(|(_, block)| block.insts.len()).sum()
}

/// Times each pass of the shared body chain and records its inline work, for
/// `rock backend-stats`.
pub(super) struct ChainStats<'a> {
    pub(super) times: &'a mut PhaseTimes,
    pub(super) work: &'a mut InlineWork,
    /// Root splices count as ordinary (a declaration, not a specialization).
    pub(super) ordinary: bool,
    /// The ordinary instance the root scan is credited to.
    pub(super) ordinary_key: Option<Instance>,
    started: Instant,
    before: (usize, usize, usize),
}

impl<'a> ChainStats<'a> {
    pub(super) fn new(times: &'a mut PhaseTimes, work: &'a mut InlineWork) -> Self {
        Self {
            times,
            work,
            ordinary: false,
            ordinary_key: None,
            started: Instant::now(),
            before: (0, 0, 0),
        }
    }
}

impl skull_transform::body_chain::PassObserver for ChainStats<'_> {
    fn begin(&mut self, pass: skull_transform::body_chain::BodyPass, mf: &MirFunc) {
        use skull_transform::body_chain::BodyPass;
        self.before = match pass {
            BodyPass::Inline | BodyPass::HoistedInline => {
                (mf.insts.len(), mf.blocks.len(), mf.cells.len())
            }
            BodyPass::Cleanup => (mir_live_instructions(mf), 0, 0),
            BodyPass::Closure => (mf.nested_fns.len(), 0, 0),
            BodyPass::CountedLoop => (mf.blocks.len(), 0, 0),
        };
        self.started = Instant::now();
    }

    fn inlined(
        &mut self,
        pass: skull_transform::body_chain::BodyPass,
        records: &[skull_transform::SpliceRecord],
        mut scan: skull_transform::InlineScanStats,
    ) {
        use skull_transform::body_chain::BodyPass;
        self.work.add_records(records);
        if pass == BodyPass::HoistedInline {
            self.work.hoisted_splices += records.len();
            self.times.hoisted_inline_work |= !records.is_empty();
        } else {
            if let Some(instance) = &self.ordinary_key {
                scan.ordinary_keys.push(instance.clone());
            }
            if self.ordinary {
                self.work.ordinary_splices += records.len();
            }
            self.times.inline_work |= !records.is_empty();
        }
        self.work.add_scan(scan);
    }

    fn load_store(&mut self) -> Option<&mut skull_transform::LoadStoreCensus> {
        Some(&mut self.work.census.load_store)
    }

    fn end(&mut self, pass: skull_transform::body_chain::BodyPass, mf: &MirFunc) {
        use skull_transform::body_chain::BodyPass;
        let elapsed = self.started.elapsed();
        let now = (mf.insts.len(), mf.blocks.len(), mf.cells.len());
        match pass {
            BodyPass::Inline => {
                self.times.inline += elapsed;
                self.work.add_allocations(self.before, now);
            }
            BodyPass::HoistedInline => {
                self.times.hoisted_inline += elapsed;
                self.work.add_allocations(self.before, now);
            }
            BodyPass::Cleanup => {
                let after = mir_live_instructions(mf);
                self.times.cleanup += elapsed;
                self.times.cleanup_work |= after != self.before.0;
                self.work.cleanup_live_before += self.before.0;
                self.work.cleanup_live_after += after;
            }
            BodyPass::Closure => {
                self.times.closure += elapsed;
                self.times.closure_work |= mf.nested_fns.len() > self.before.0;
            }
            BodyPass::CountedLoop => {
                self.times.counted_loop += elapsed;
                self.times.counted_loop_work |= mf.blocks.len() > self.before.0;
            }
        }
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn record_phase(
    phase: &mut AttributionPhase,
    samples: &mut Vec<u64>,
    duration: Duration,
    did_work: bool,
    work_volume: usize,
    instructions: usize,
) {
    phase.bodies_examined += 1;
    phase.elapsed += duration;
    phase.work_volume += work_volume;
    phase.instructions_visited += instructions;
    if duration > Duration::ZERO {
        samples.push(duration_us(duration));
    }
    if did_work {
        phase.bodies_with_work += 1;
    }
}

fn finish_distribution(phase: &mut AttributionPhase, samples: &mut [u64]) {
    samples.sort_unstable();
    if samples.is_empty() {
        return;
    }
    let at = |percentile: usize| samples[(samples.len() - 1) * percentile / 100];
    phase.p50_us = at(50);
    phase.p95_us = at(95);
    phase.max_us = *samples.last().expect("nonempty duration sample");
}

impl AttributionAccumulator {
    pub(super) fn add_unit(&mut self, unit: &UnitAttribution) {
        if !unit.examined {
            return;
        }
        self.result.bodies_examined += 1;
        if unit.mir_instructions > 0 {
            self.result.bodies_with_work += 1;
        }
        if self.census_enabled {
            if let Some((instance, kind)) = &unit.consumer_instance {
                match kind {
                    LoweredInstanceKind::Ordinary => {
                        self.ordinary_consumer_keys.insert(instance.clone());
                        if unit.canonical_reused {
                            self.ordinary_canonical_reuse_keys.insert(instance.clone());
                        }
                    }
                    LoweredInstanceKind::Specialized => {
                        self.specialized_consumer_keys.insert(instance.clone());
                        if unit.canonical_reused {
                            self.specialized_canonical_reuse_keys
                                .insert(instance.clone());
                        }
                    }
                }
                if unit.canonical_reused {
                    self.canonical_clone_derivations += 1;
                }
            }
            if let Some((instance, kind)) = &unit.lowered_instance {
                match kind {
                    LoweredInstanceKind::Ordinary => {
                        self.ordinary_lowered_keys.insert(instance.clone());
                        self.ordinary_lower_ns_by_key
                            .entry(instance.clone())
                            .or_default()
                            .push(duration_ns(unit.times.lower));
                    }
                    LoweredInstanceKind::Specialized => {
                        self.specialized_lowered_keys.insert(instance.clone());
                        self.specialized_lower_ns_by_key
                            .entry(instance.clone())
                            .or_default()
                            .push(duration_ns(unit.times.lower));
                    }
                }
            }
            if unit.always_lowered {
                self.always_lowered_units += 1;
            }
            self.always_callee_keys
                .extend(unit.always_callee_keys.iter().cloned());
        }
        let times = &unit.times;
        record_phase(
            &mut self.result.lower,
            &mut self.lower_samples,
            times.lower,
            times.lower_work,
            unit.hir_nodes,
            unit.mir_instructions,
        );
        self.result.lower.concrete_mono_requests += unit.lower_facts.concrete_mono_requests;
        self.result.lower.concrete_mono_cache_hits += unit.lower_facts.concrete_mono_cache_hits;
        self.result.lower.concrete_mono_derivations += unit.lower_facts.concrete_mono_derivations;
        self.result.lower.concrete_mono_unique_ids += unit.lower_facts.concrete_mono_unique_ids;
        self.result.lower.concrete_mono_cloned_nodes += unit.lower_facts.concrete_mono_cloned_nodes;
        self.result.lower.concrete_mono_interner_miss_cloned_nodes +=
            unit.lower_facts.concrete_mono_interner_miss_cloned_nodes;
        self.result.lower.concrete_mono_cache_insertion_cloned_nodes +=
            unit.lower_facts.concrete_mono_cache_insertion_cloned_nodes;
        self.result.lower.concrete_mono_cache_hit_cloned_nodes +=
            unit.lower_facts.concrete_mono_cache_hit_cloned_nodes;
        self.result
            .lower
            .concrete_mono_explicit_owned_escape_cloned_nodes += unit
            .lower_facts
            .concrete_mono_explicit_owned_escape_cloned_nodes;
        self.result.lower.concrete_mono_explicit_owned_escapes +=
            unit.lower_facts.concrete_mono_explicit_owned_escapes;
        self.result.lower.concrete_mono_lookup_ns += unit.lower_facts.concrete_mono_lookup_ns;
        self.result.lower.concrete_mono_cache_lookup_ns +=
            unit.lower_facts.concrete_mono_cache_lookup_ns;
        self.result.lower.concrete_mono_shared_handle_requests +=
            unit.lower_facts.concrete_mono_shared_handle_requests;
        self.result.lower.concrete_mono_clone_self_ns +=
            unit.lower_facts.concrete_mono_clone_self_ns;
        self.result.lower.concrete_mono_timed_requests +=
            unit.lower_facts.concrete_mono_timed_requests;
        for (total, child) in self
            .result
            .lower
            .concrete_mono_by_consumer
            .iter_mut()
            .zip(unit.lower_facts.concrete_mono_by_consumer.iter())
        {
            total.requests += child.requests;
            total.cloned_nodes += child.cloned_nodes;
            total.explicit_owned_escapes += child.explicit_owned_escapes;
        }
        self.result.lower.abi_param_copy_volume += unit.lower_facts.abi_param_copy_volume;
        self.result.lower.abi_param_shared_views += unit.lower_facts.abi_param_shared_views;
        self.result.lower.mir_instruction_allocations +=
            unit.lower_facts.mir_instruction_allocations;
        self.result.lower.mir_instruction_reallocations +=
            unit.lower_facts.mir_instruction_reallocations;
        self.result.lower.mir_instruction_reallocation_ns +=
            unit.lower_facts.mir_instruction_reallocation_ns;
        self.result.lower.mir_instruction_capacity += unit.lower_facts.mir_instruction_capacity;
        self.result.lower.mir_cell_allocations += unit.lower_facts.mir_cell_allocations;
        self.result.lower.mir_cell_reallocations += unit.lower_facts.mir_cell_reallocations;
        self.result.lower.mir_cell_reallocation_ns += unit.lower_facts.mir_cell_reallocation_ns;
        self.result.lower.mir_cell_capacity += unit.lower_facts.mir_cell_capacity;
        self.result.lower.mir_block_allocations += unit.lower_facts.mir_block_allocations;
        self.result.lower.mir_block_reallocations += unit.lower_facts.mir_block_reallocations;
        self.result.lower.mir_block_reallocation_ns += unit.lower_facts.mir_block_reallocation_ns;
        self.result.lower.mir_block_capacity += unit.lower_facts.mir_block_capacity;
        record_phase(
            &mut self.result.inline_phase,
            &mut self.inline_samples,
            times.inline,
            times.inline_work,
            unit.inline.accepted_splices,
            unit.inline.copied_instructions,
        );
        record_phase(
            &mut self.result.cleanup,
            &mut self.cleanup_samples,
            times.cleanup,
            times.cleanup_work,
            unit.mir_instructions,
            0,
        );
        record_phase(
            &mut self.result.closure,
            &mut self.closure_samples,
            times.closure,
            times.closure_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.counted_loop,
            &mut self.counted_loop_samples,
            times.counted_loop,
            times.counted_loop_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.hoisted_inline,
            &mut self.hoisted_inline_samples,
            times.hoisted_inline,
            times.hoisted_inline_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.validation,
            &mut self.validation_samples,
            times.validation,
            times.validation_work,
            0,
            0,
        );
        record_phase(
            &mut self.result.regalloc,
            &mut self.regalloc_samples,
            times.regalloc,
            times.regalloc_work,
            0,
            0,
        );
        let stringify = times.emit.saturating_sub(times.regalloc);
        record_phase(
            &mut self.result.stringify,
            &mut self.stringify_samples,
            stringify,
            times.emit_work,
            unit.mir_instructions,
            0,
        );
        self.result.inline.accepted_splices += unit.inline.accepted_splices;
        self.result.inline.copied_instructions += unit.inline.copied_instructions;
        self.result.inline.ordinary_splices += unit.inline.ordinary_splices;
        self.result.inline.specialized_splices += unit.inline.specialized_splices;
        self.result.inline.bounded_splices += unit.inline.bounded_splices;
        self.result.inline.hoisted_splices += unit.inline.hoisted_splices;
        self.result.inline.arena_allocations += unit.inline.arena_allocations;
        self.result.inline.cleanup_live_before += unit.inline.cleanup_live_before;
        self.result.inline.cleanup_live_after += unit.inline.cleanup_live_after;
        self.result.inline.borrowed_block_scans += unit.inline.borrowed_block_scans;
        self.result.inline.instructions_scanned += unit.inline.instructions_scanned;
        self.result.inline.census.add(unit.inline.census.clone());
        for (owner, (splices, copied, splice_ns, live, dead)) in &unit.inline.owners {
            let entry = self
                .result
                .inline
                .top_owners
                .iter_mut()
                .find(|item| item.owner == *owner);
            if let Some(entry) = entry {
                entry.splices += *splices;
                entry.copied_instructions += *copied;
                entry.splice_ns += *splice_ns;
                entry.live_cloned_entries += *live;
                entry.dead_cloned_entries += *dead;
            } else {
                self.result.inline.top_owners.push(InlineOwnerAttribution {
                    owner: owner.clone(),
                    splices: *splices,
                    copied_instructions: *copied,
                    splice_ns: *splice_ns,
                    live_cloned_entries: *live,
                    dead_cloned_entries: *dead,
                });
            }
        }
        self.top_bodies.push(BodyAttributionOwner {
            owner: unit.owner.clone(),
            lower_us: duration_us(times.lower),
            mir_instructions: unit.mir_instructions,
            concrete_mono_requests: unit.lower_facts.concrete_mono_requests,
            concrete_mono_cloned_nodes: unit.lower_facts.concrete_mono_cloned_nodes,
        });
    }

    pub(super) fn finish(mut self) -> EmitAttribution {
        finish_distribution(&mut self.result.lower, &mut self.lower_samples);
        finish_distribution(&mut self.result.inline_phase, &mut self.inline_samples);
        finish_distribution(&mut self.result.cleanup, &mut self.cleanup_samples);
        finish_distribution(&mut self.result.closure, &mut self.closure_samples);
        finish_distribution(
            &mut self.result.counted_loop,
            &mut self.counted_loop_samples,
        );
        finish_distribution(
            &mut self.result.hoisted_inline,
            &mut self.hoisted_inline_samples,
        );
        finish_distribution(&mut self.result.validation, &mut self.validation_samples);
        finish_distribution(&mut self.result.regalloc, &mut self.regalloc_samples);
        finish_distribution(&mut self.result.stringify, &mut self.stringify_samples);
        self.result.inline.top_owners.sort_by(|a, b| {
            b.copied_instructions
                .cmp(&a.copied_instructions)
                .then_with(|| a.owner.cmp(&b.owner))
        });
        self.result.inline.top_owners.truncate(8);
        self.result.inline.candidate_unique_keys = self
            .result
            .inline
            .census
            .candidate_requested_keys
            .iter()
            .collect::<FxHashSet<_>>()
            .len();
        let ordinary_keys: FxHashSet<_> = self.result.inline.census.ordinary_keys.iter().collect();
        self.result.inline.candidate_snapshot_duplicates = self
            .result
            .inline
            .census
            .candidate_keys
            .iter()
            .filter(|key| ordinary_keys.contains(key))
            .count();
        if self.census_enabled {
            let published = &self.candidate_published_keys;
            let requested: FxHashSet<_> = self
                .result
                .inline
                .census
                .candidate_requested_keys
                .iter()
                .cloned()
                .collect();
            let spliced: FxHashSet<_> = self
                .result
                .inline
                .census
                .spliced_keys
                .iter()
                .cloned()
                .collect();
            let mut lowerings_by_key: BTreeMap<Instance, Vec<u64>> = BTreeMap::new();
            for (key, times) in &self.candidate_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            for (key, times) in &self.ordinary_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            for (key, times) in &self.specialized_lower_ns_by_key {
                lowerings_by_key
                    .entry(key.clone())
                    .or_default()
                    .extend(times.iter().copied());
            }
            let mut keys_lowered_more_than_once = 0;
            let mut first_lower_ns = 0;
            let mut duplicate_lower_ns = 0;
            for times in lowerings_by_key.values() {
                if times.len() > 1 {
                    keys_lowered_more_than_once += 1;
                }
                if let Some((first, rest)) = times.split_first() {
                    first_lower_ns += *first;
                    duplicate_lower_ns += rest.iter().sum::<u64>();
                }
            }
            let ordinary_lowering_derivations =
                self.ordinary_lower_ns_by_key.values().map(Vec::len).sum();
            let specialized_lowering_derivations = self
                .specialized_lower_ns_by_key
                .values()
                .map(Vec::len)
                .sum();
            let candidate_ordinary_consumer_overlap = self
                .candidate_eligible_keys
                .intersection(&self.ordinary_consumer_keys)
                .count();
            let candidate_specialized_consumer_overlap = self
                .candidate_eligible_keys
                .intersection(&self.specialized_consumer_keys)
                .count();
            let ordinary_canonical_miss_instances: Vec<_> = self
                .candidate_eligible_keys
                .intersection(&self.ordinary_consumer_keys)
                .filter(|key| !self.ordinary_canonical_reuse_keys.contains(*key))
                .cloned()
                .collect();
            let specialized_canonical_miss_instances: Vec<_> = self
                .candidate_eligible_keys
                .intersection(&self.specialized_consumer_keys)
                .filter(|key| !self.specialized_canonical_reuse_keys.contains(*key))
                .cloned()
                .collect();
            let population = &mut self.result.inline.population;
            population.candidate_published_keys = published.len();
            population.candidate_eligible_keys = self.candidate_eligible_keys.len();
            population.ordinary_lowered_keys = self.ordinary_lowered_keys.len();
            population.specialized_lowered_keys = self.specialized_lowered_keys.len();
            population.ordinary_lowering_derivations = ordinary_lowering_derivations;
            population.specialized_lowering_derivations = specialized_lowering_derivations;
            population.ordinary_consumer_keys = self.ordinary_consumer_keys.len();
            population.specialized_consumer_keys = self.specialized_consumer_keys.len();
            population.candidate_ordinary_consumer_overlap = candidate_ordinary_consumer_overlap;
            population.candidate_specialized_consumer_overlap =
                candidate_specialized_consumer_overlap;
            population.ordinary_canonical_reuse_keys = self.ordinary_canonical_reuse_keys.len();
            population.specialized_canonical_reuse_keys =
                self.specialized_canonical_reuse_keys.len();
            population.ordinary_canonical_miss_keys = ordinary_canonical_miss_instances.len();
            population.specialized_canonical_miss_keys = specialized_canonical_miss_instances.len();
            population.ordinary_canonical_miss_instances = ordinary_canonical_miss_instances;
            population.specialized_canonical_miss_instances = specialized_canonical_miss_instances;
            population.canonical_clone_derivations = self.canonical_clone_derivations;
            population.always_lowered_units = self.always_lowered_units;
            population.always_callee_keys = self.always_callee_keys.len();
            population.candidate_ordinary_overlap =
                published.intersection(&self.ordinary_lowered_keys).count();
            population.candidate_specialized_overlap = published
                .intersection(&self.specialized_lowered_keys)
                .count();
            population.candidate_always_callee_overlap =
                published.intersection(&self.always_callee_keys).count();
            population.ordinary_specialized_overlap = self
                .ordinary_lowered_keys
                .intersection(&self.specialized_lowered_keys)
                .count();
            population.candidate_requested_unique_keys = requested.len();
            population.candidate_spliced_unique_keys = spliced.len();
            population.candidate_published_never_requested =
                published.difference(&requested).count();
            population.candidate_requested_never_spliced = requested.difference(&spliced).count();
            population.candidate_requested_not_published = requested.difference(published).count();
            population.candidate_lower_failed = population
                .candidate_lower_attempted
                .saturating_sub(population.candidate_lowered_keys);
            population.candidate_rejected_after_lower = population
                .candidate_lowered_keys
                .saturating_sub(population.candidate_eligible_keys);
            population.keys_lowered_more_than_once = keys_lowered_more_than_once;
            population.first_lower_ns = first_lower_ns;
            population.duplicate_lower_ns = duplicate_lower_ns;
        }
        self.top_bodies.sort_by(|a, b| {
            b.lower_us
                .cmp(&a.lower_us)
                .then_with(|| a.owner.cmp(&b.owner))
        });
        self.top_bodies.truncate(8);
        self.result.top_bodies = self.top_bodies;
        self.result
    }
}
