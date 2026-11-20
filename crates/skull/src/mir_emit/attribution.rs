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

impl InlineAttribution {
    fn add_records(&mut self, records: &[skull_transform::SpliceRecord]) {
        self.accepted_splices += records.len();
        self.copied_instructions += records
            .iter()
            .map(|record| record.callee_insts)
            .sum::<usize>();
    }

    pub(super) fn add(&mut self, other: &Self) {
        self.accepted_splices += other.accepted_splices;
        self.copied_instructions += other.copied_instructions;
        self.cleanup_live_before += other.cleanup_live_before;
        self.cleanup_live_after += other.cleanup_live_after;
    }
}

#[derive(Default, Clone)]
pub(super) struct UnitAttribution {
    pub(super) owner: String,
    pub(super) examined: bool,
    pub(super) times: PhaseTimes,
    pub(super) hir_nodes: usize,
    pub(super) mir_instructions: usize,
    pub(super) inline: InlineAttribution,
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
    pub(super) work: &'a mut InlineAttribution,
    started: Instant,
    before: usize,
}

impl<'a> ChainStats<'a> {
    pub(super) fn new(times: &'a mut PhaseTimes, work: &'a mut InlineAttribution) -> Self {
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
    pub(super) fn add_unit(&mut self, unit: &UnitAttribution) {
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
        self.result.inline.add(&unit.inline);
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
