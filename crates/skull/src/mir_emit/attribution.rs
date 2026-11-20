use super::*;

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
    pub bounds: AttributionPhase,
    pub hoisted_inline: AttributionPhase,
    pub validation: AttributionPhase,
    pub regalloc: AttributionPhase,
    pub stringify: AttributionPhase,
    pub top_bodies: Vec<BodyAttributionOwner>,
    /// One record per body assembly folded, in fold order.
    pub bodies: Vec<BodyCost>,
}

/// The source definition an emitted body belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BodySource {
    #[default]
    Unknown,
    Function(FuncRef),
    Global(ModuleId, Idx<bone_semantic::hir::GlobalDef>),
    Decorator(ModuleId, Idx<bone_semantic::hir::DecoratorDef>),
}

/// One callee spliced into a body: how often, and the callee instructions
/// copied in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpliceCost {
    pub callee: FuncRef,
    pub count: usize,
    pub instructions: usize,
}

/// One emitted body's own costs.
#[derive(Clone, Debug, Default)]
pub struct BodyCost {
    pub source: BodySource,
    /// A specialization's Lua name; empty for a declaration's own body.
    pub symbol: String,
    /// Whether the body's text is in the output (assembly can drop one).
    pub emitted: bool,
    /// Lowering reused the inline candidate's body instead of lowering again.
    pub canonical_reused: bool,
    pub thread_cpu: Option<Duration>,
    pub lower: Duration,
    pub inline: Duration,
    pub cleanup: Duration,
    pub closure: Duration,
    pub counted_loop: Duration,
    pub bounds: Duration,
    pub bounds_elided: usize,
    pub hoisted_inline: Duration,
    pub validation: Duration,
    /// Lua printing, register allocation included.
    pub emit: Duration,
    pub regalloc: Duration,
    pub hir_nodes: usize,
    /// MIR instructions lowering produced, before any splice.
    pub mir_lowered: usize,
    /// Live MIR instructions entering and leaving cleanup, summed over the
    /// body's cleanup runs: after splicing, and as printed.
    pub mir_inlined: usize,
    pub mir_final: usize,
    pub splices: Vec<SpliceCost>,
    pub lua_bytes: usize,
    pub lua_lines: usize,
    /// The body's wire, parameters then result; empty without an ABI plan.
    pub wire: Vec<skull_mir::lower::WirePosition>,
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
}

#[derive(Clone, Debug, Default)]
pub struct InlineAttribution {
    pub accepted_splices: usize,
    pub copied_instructions: usize,
    pub cleanup_live_before: usize,
    pub cleanup_live_after: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BodyAttributionOwner {
    pub owner: String,
    pub lower_us: u64,
    pub mir_instructions: usize,
}

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
    pub(super) bounds: Duration,
    pub(super) bounds_elided: usize,
    pub(super) hoisted_inline: Duration,
    pub(super) hoisted_inline_work: bool,
    pub(super) validation: Duration,
    pub(super) validation_work: bool,
    pub(super) emit: Duration,
    pub(super) emit_work: bool,
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
        self.bounds += o.bounds;
        self.bounds_elided += o.bounds_elided;
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
    accepted_splices: usize,
    copied_instructions: usize,
    cleanup_live_before: usize,
    cleanup_live_after: usize,
    /// Splice count and copied instructions per callee.
    splices: FxHashMap<FuncRef, (usize, usize)>,
}

impl InlineWork {
    fn add_records(&mut self, records: &[skull_transform::SpliceRecord]) {
        self.accepted_splices += records.len();
        for record in records {
            self.copied_instructions += record.callee_insts;
            let entry = self.splices.entry(record.callee).or_default();
            entry.0 += 1;
            entry.1 += record.callee_insts;
        }
    }

    pub(super) fn add(&mut self, other: InlineWork) {
        self.accepted_splices += other.accepted_splices;
        self.copied_instructions += other.copied_instructions;
        self.cleanup_live_before += other.cleanup_live_before;
        self.cleanup_live_after += other.cleanup_live_after;
        for (callee, (count, copied)) in other.splices {
            let entry = self.splices.entry(callee).or_default();
            entry.0 += count;
            entry.1 += copied;
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct UnitAttribution {
    pub(super) owner: String,
    pub(super) source: BodySource,
    pub(super) mir_lowered: usize,
    pub(super) thread_cpu: Option<Duration>,
    pub(super) examined: bool,
    pub(super) times: PhaseTimes,
    pub(super) hir_nodes: usize,
    pub(super) mir_instructions: usize,
    pub(super) inline: InlineWork,
    pub(super) consumer_instance: Option<(Instance, LoweredInstanceKind)>,
    pub(super) canonical_reused: bool,
    pub(super) abi_plan: Option<skull_mir::lower::BackendFnAbiPlanHandle>,
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
    pub(super) bounds_samples: Vec<u64>,
    pub(super) hoisted_inline_samples: Vec<u64>,
    pub(super) validation_samples: Vec<u64>,
    pub(super) regalloc_samples: Vec<u64>,
    pub(super) stringify_samples: Vec<u64>,
    pub(super) top_bodies: Vec<BodyAttributionOwner>,
    pub(super) enabled: bool,
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

pub(super) struct ChainStats<'a> {
    pub(super) times: &'a mut PhaseTimes,
    pub(super) work: &'a mut InlineWork,
    started: Instant,
    before: usize,
}

impl<'a> ChainStats<'a> {
    pub(super) fn new(times: &'a mut PhaseTimes, work: &'a mut InlineWork) -> Self {
        Self {
            times,
            work,
            started: Instant::now(),
            before: 0,
        }
    }
}

impl skull_transform::body_chain::PassObserver for ChainStats<'_> {
    fn begin(&mut self, pass: skull_transform::body_chain::BodyPass, mf: &MirFunc) {
        use skull_transform::body_chain::BodyPass;
        self.before = match pass {
            BodyPass::Cleanup => mir_live_instructions(mf),
            BodyPass::Closure => mf.nested_fns.len(),
            BodyPass::CountedLoop => mf.blocks.len(),
            BodyPass::Inline | BodyPass::HoistedInline | BodyPass::Bounds => 0,
        };
        self.started = Instant::now();
    }

    fn inlined(
        &mut self,
        pass: skull_transform::body_chain::BodyPass,
        records: &[skull_transform::SpliceRecord],
    ) {
        use skull_transform::body_chain::BodyPass;
        self.work.add_records(records);
        if pass == BodyPass::HoistedInline {
            self.times.hoisted_inline_work |= !records.is_empty();
        } else {
            self.times.inline_work |= !records.is_empty();
        }
    }

    fn bounds_elided(&mut self, count: usize) {
        self.times.bounds_elided += count;
    }

    fn end(&mut self, pass: skull_transform::body_chain::BodyPass, mf: &MirFunc) {
        use skull_transform::body_chain::BodyPass;
        let elapsed = self.started.elapsed();
        match pass {
            BodyPass::Inline => self.times.inline += elapsed,
            BodyPass::HoistedInline => self.times.hoisted_inline += elapsed,
            BodyPass::Cleanup => {
                let after = mir_live_instructions(mf);
                self.times.cleanup += elapsed;
                self.times.cleanup_work |= after != self.before;
                self.work.cleanup_live_before += self.before;
                self.work.cleanup_live_after += after;
            }
            BodyPass::Closure => {
                self.times.closure += elapsed;
                self.times.closure_work |= mf.nested_fns.len() > self.before;
            }
            BodyPass::CountedLoop => {
                self.times.counted_loop += elapsed;
                self.times.counted_loop_work |= mf.blocks.len() > self.before;
            }
            BodyPass::Bounds => self.times.bounds += elapsed,
        }
    }
}

/// The calling thread's CPU time; zero where the platform has no such clock.
pub(super) fn thread_cpu_time() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the call's duration.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    Duration::ZERO
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
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
    pub(super) fn add_body(&mut self, unit: &UnitAttribution, lua: Option<&str>) {
        if !self.enabled || !unit.examined {
            return;
        }
        self.add_body_sized(unit, lua.map(|lua| (lua.len(), lua.lines().count())));
    }

    pub(super) fn add_body_sized(&mut self, unit: &UnitAttribution, text: Option<(usize, usize)>) {
        if !self.enabled || !unit.examined {
            return;
        }
        self.add_unit(unit);
        let times = &unit.times;
        let source = match (&unit.source, &unit.consumer_instance) {
            (BodySource::Unknown, Some((instance, _))) => BodySource::Function(instance.func),
            (source, _) => *source,
        };
        let specialized = matches!(
            unit.consumer_instance,
            Some((_, LoweredInstanceKind::Specialized))
        );
        let mut splices: Vec<SpliceCost> = unit
            .inline
            .splices
            .iter()
            .map(|(callee, (count, instructions))| SpliceCost {
                callee: *callee,
                count: *count,
                instructions: *instructions,
            })
            .collect();
        splices.sort_by_key(|s| (s.callee.module.index(), u32::from(s.callee.func.into_raw())));
        self.result.bodies.push(BodyCost {
            source,
            symbol: if specialized {
                unit.owner.clone()
            } else {
                String::new()
            },
            emitted: text.is_some(),
            canonical_reused: unit.canonical_reused,
            thread_cpu: unit.thread_cpu,
            lower: times.lower,
            inline: times.inline,
            cleanup: times.cleanup,
            closure: times.closure,
            counted_loop: times.counted_loop,
            bounds: times.bounds,
            bounds_elided: times.bounds_elided,
            hoisted_inline: times.hoisted_inline,
            validation: times.validation,
            emit: times.emit,
            regalloc: times.regalloc,
            hir_nodes: unit.hir_nodes,
            mir_lowered: unit.mir_lowered,
            mir_inlined: unit.inline.cleanup_live_before,
            mir_final: unit.inline.cleanup_live_after,
            splices,
            lua_bytes: text.map_or(0, |(bytes, _)| bytes),
            lua_lines: text.map_or(0, |(_, lines)| lines),
            wire: unit
                .abi_plan
                .as_ref()
                .map(|plan| plan.wire_positions())
                .unwrap_or_default(),
        });
    }

    fn add_unit(&mut self, unit: &UnitAttribution) {
        if !self.enabled || !unit.examined {
            return;
        }
        self.result.bodies_examined += 1;
        if unit.mir_instructions > 0 {
            self.result.bodies_with_work += 1;
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
            &mut self.result.bounds,
            &mut self.bounds_samples,
            times.bounds,
            times.bounds_elided > 0,
            times.bounds_elided,
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
        self.result.inline.cleanup_live_before += unit.inline.cleanup_live_before;
        self.result.inline.cleanup_live_after += unit.inline.cleanup_live_after;
        self.top_bodies.push(BodyAttributionOwner {
            owner: unit.owner.clone(),
            lower_us: duration_us(times.lower),
            mir_instructions: unit.mir_instructions,
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
        finish_distribution(&mut self.result.bounds, &mut self.bounds_samples);
        finish_distribution(
            &mut self.result.hoisted_inline,
            &mut self.hoisted_inline_samples,
        );
        finish_distribution(&mut self.result.validation, &mut self.validation_samples);
        finish_distribution(&mut self.result.regalloc, &mut self.regalloc_samples);
        finish_distribution(&mut self.result.stringify, &mut self.stringify_samples);
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
