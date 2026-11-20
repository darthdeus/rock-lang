//! Time the Rock front end and Skull emission over one shared source set.

use crate::Result;
use crate::bone_backend::{self, BonePhaseTimes};
use crate::skull_backend::{self, SkullPhaseTimes};
use colored::Colorize;
use rock_core::SourceFiles;
use std::time::Duration;

/// Front-end and emit timings, summed across timed iterations. Each iteration
/// runs the front end once and emits the workspace it checked.
#[derive(Default)]
struct Stats {
    files: usize,
    front: BonePhaseTimes,
    /// Bone's own phase split of `front.semantic`, in run order.
    front_phases: Vec<(&'static str, Duration)>,
    checked: bool,
    emit: SkullPhaseTimes,
    /// Per-unit timers overlap under parallel emission. Setup timers measure wall time.
    emit_phases: Vec<(&'static str, Duration)>,
    abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    emitted: bool,
    failure: Option<rock_core::CompilerError>,
}

/// Accumulate one run's ordered phase list into `acc`. Each run reports the
/// same phases in the same order, so they are summed element-wise by position.
fn merge_phases(acc: &mut Vec<(&'static str, Duration)>, new: &[(&'static str, Duration)]) {
    if acc.is_empty() {
        acc.extend_from_slice(new);
    } else {
        for (a, n) in acc.iter_mut().zip(new) {
            a.1 += n.1;
        }
    }
}

pub(crate) fn do_backend_stats(input: &str, iters: usize, warmup: usize, json: bool) -> Result<()> {
    let iters = iters.max(1);
    let project_path = std::path::Path::new(input);

    let sources = rock_host::compilation_units::load_sources(project_path)?;
    if sources.files.is_empty() {
        anyhow::bail!("no Rock source files found at `{}`", input);
    }

    let stats = run(&sources, project_path, iters, warmup);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_report(input, iters, warmup, &stats))?
        );
    } else {
        print_report(input, iters, warmup, &stats);
    }
    if stats.emit.setup_failed || stats.emit.emit_failed {
        let stage = if stats.emit.setup_failed {
            "workspace setup"
        } else {
            "emit"
        };
        if let Some(error) = &stats.failure {
            anyhow::bail!("backend-stats skull {stage} failed: {error}");
        }
        anyhow::bail!("backend-stats skull {stage} failed without a diagnostic");
    }
    Ok(())
}

fn run(
    sources: &SourceFiles,
    project_path: &std::path::Path,
    iters: usize,
    warmup: usize,
) -> Stats {
    let mut stats = Stats {
        files: sources.files.len(),
        ..Default::default()
    };
    for i in 0..(warmup + iters) {
        let timed = i >= warmup;
        let (frontend, front) = bone_backend::workspace_frontend_timed(sources);
        if timed {
            stats.front.parse += front.parse;
            stats.front.semantic += front.semantic;
            stats.front.stdlib_assemble += front.stdlib_assemble;
            stats.front.semantic_call += front.semantic_call;
        }
        let (ws, entry_file) = match frontend {
            Ok(checked) => checked,
            Err(mut errors) => {
                stats.checked = false;
                stats.failure = errors.drain(..).next();
                continue;
            }
        };
        if timed {
            merge_phases(&mut stats.front_phases, &ws.phase_timings);
        }
        // Body diagnostics can exist in a successful semantic-pass result.
        if let Some(error) = bone_backend::body_errors(&ws).into_iter().next() {
            stats.checked = false;
            stats.failure = Some(error);
            continue;
        }
        stats.checked = true;
        let (ok, emit) = skull_backend::skull_emit_timed(ws, entry_file, project_path);
        stats.emitted = ok;
        if !timed {
            continue;
        }
        stats.failure = emit.failure.clone();
        merge_phases(&mut stats.emit_phases, &emit.emit_phases);
        stats.abi_plan_stats.merge(&emit.abi_plan_stats);
        let (setup, emit_time) = (stats.emit.workspace_setup, stats.emit.emit);
        stats.emit = emit;
        stats.emit.workspace_setup += setup;
        stats.emit.emit += emit_time;
    }
    stats
}

fn print_attribution(attribution: &skull::mir_emit::EmitAttribution) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let phase = |name: &str, p: &skull::mir_emit::AttributionPhase| {
        println!(
            "    {:24} {:>7.3} ms; bodies {}/{}; work {}; instr {}; p50/p95/max {}/{}/{} us",
            name,
            ms(p.elapsed),
            p.bodies_with_work,
            p.bodies_examined,
            p.work_volume,
            p.instructions_visited,
            p.p50_us,
            p.p95_us,
            p.max_us,
        );
    };

    println!(
        "  body attribution (one timed run): {} examined, {} with MIR work",
        attribution.bodies_examined, attribution.bodies_with_work
    );
    phase("lower hir->mir", &attribution.lower);
    phase("inline plan+splice", &attribution.inline_phase);
    phase("cleanup mir", &attribution.cleanup);
    phase("closure conversion", &attribution.closure);
    phase("counted-loop recovery", &attribution.counted_loop);
    phase("bounds-check elimination", &attribution.bounds);
    phase("hoisted-body inline", &attribution.hoisted_inline);
    phase("post-transform validation", &attribution.validation);
    phase("regalloc", &attribution.regalloc);
    phase("stringify", &attribution.stringify);
    println!(
        "    lower facts: ABI param copied slots {}, shared views {}; MIR inst/cell/block alloc {}/{}/{} realloc {}/{}/{} capacity {}/{}/{}",
        attribution.lower.abi_param_copy_volume,
        attribution.lower.abi_param_shared_views,
        attribution.lower.mir_instruction_allocations,
        attribution.lower.mir_cell_allocations,
        attribution.lower.mir_block_allocations,
        attribution.lower.mir_instruction_reallocations,
        attribution.lower.mir_cell_reallocations,
        attribution.lower.mir_block_reallocations,
        attribution.lower.mir_instruction_capacity,
        attribution.lower.mir_cell_capacity,
        attribution.lower.mir_block_capacity,
    );
    println!(
        "    MIR reallocation self: inst/cell/block {:.3}/{:.3}/{:.3} ms (opt-in timing)",
        attribution.lower.mir_instruction_reallocation_ns as f64 / 1_000_000.0,
        attribution.lower.mir_cell_reallocation_ns as f64 / 1_000_000.0,
        attribution.lower.mir_block_reallocation_ns as f64 / 1_000_000.0,
    );
    println!(
        "    inline results: {} accepted splices, {} copied instructions, {} arena allocations; cleanup live before/after {}/{}; {} borrowed block scans over {} instructions",
        attribution.inline.accepted_splices,
        attribution.inline.copied_instructions,
        attribution.inline.arena_allocations,
        attribution.inline.cleanup_live_before,
        attribution.inline.cleanup_live_after,
        attribution.inline.borrowed_block_scans,
        attribution.inline.instructions_scanned,
    );
    let census = &attribution.inline.census;
    if census.enabled {
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        println!(
            "    inline census: blocks {:.3} ms/{}; calls {}/{}; resume {}, rescanned {}",
            ms(census.top_level_blocks_ns),
            census.top_level_blocks_population,
            census.call_instructions_examined,
            census.call_target_resolutions,
            census.resume_cursor_progress,
            census.rescanned_instructions,
        );
        println!(
            "      candidates: requests {}, hits {}, derivations {}, unique {}, ordinary duplicates {}; lower/cleanup/eligibility {:.3}/{:.3}/{:.3} ms",
            census.candidate_requests,
            census.candidate_cache_hits,
            census.candidate_derivations,
            attribution.inline.candidate_unique_keys,
            attribution.inline.candidate_snapshot_duplicates,
            ms(census.candidate_lower_ns),
            ms(census.candidate_cleanup_ns),
            ms(census.candidate_eligibility_ns),
        );
        let population = &attribution.inline.population;
        println!(
            "      candidate table: keys {}, warmed {}, reused {}, eligible {}, rejected {}; lower/cleanup/compact/eligibility {:.3}/{:.3}/{:.3}/{:.3} ms",
            attribution.inline.candidate_setup_keys,
            attribution.inline.candidate_setup_warmed,
            attribution.inline.candidate_setup_reused,
            attribution.inline.candidate_setup_eligible,
            attribution.inline.candidate_setup_rejected,
            ms(attribution.inline.candidate_setup_lower_ns),
            ms(attribution.inline.candidate_setup_cleanup_ns),
            ms(population.candidate_compact_ns),
            ms(attribution.inline.candidate_setup_eligibility_ns),
        );
        println!(
            "      population: published {}, eligible {}, lower-attempted {}, lowered {}, lower-failed {}, rejected-before-lower {}, rejected-after-lower {}, ordinary {}, specialized {}, always units {}, always callees {}",
            population.candidate_published_keys,
            population.candidate_eligible_keys,
            population.candidate_lower_attempted,
            population.candidate_lowered_keys,
            population.candidate_lower_failed,
            population.candidate_rejected_before_lower,
            population.candidate_rejected_after_lower,
            population.ordinary_lowered_keys,
            population.specialized_lowered_keys,
            population.always_lowered_units,
            population.always_callee_keys,
        );
        println!(
            "      population overlap: candidate/ordinary {}, candidate/specialized {}, candidate/always {}, ordinary/specialized {}; lowered-more-than-once {}",
            population.candidate_ordinary_overlap,
            population.candidate_specialized_overlap,
            population.candidate_always_callee_overlap,
            population.ordinary_specialized_overlap,
            population.keys_lowered_more_than_once,
        );
        println!(
            "      canonical consumers: ordinary/specialized {}/{}; candidate overlap {}/{}; reused {}/{}; misses {}/{}; consumer clones {}",
            population.ordinary_consumer_keys,
            population.specialized_consumer_keys,
            population.candidate_ordinary_consumer_overlap,
            population.candidate_specialized_consumer_overlap,
            population.ordinary_canonical_reuse_keys,
            population.specialized_canonical_reuse_keys,
            population.ordinary_canonical_miss_keys,
            population.specialized_canonical_miss_keys,
            population.canonical_clone_derivations,
        );
        if !population.ordinary_canonical_miss_instances.is_empty()
            || !population.specialized_canonical_miss_instances.is_empty()
        {
            println!(
                "        canonical miss instances: ordinary {:?}; specialized {:?}",
                population.ordinary_canonical_miss_instances,
                population.specialized_canonical_miss_instances,
            );
        }
        println!(
            "      MIR population: raw i/c/b {}/{}/{} clean {}/{}/{}; derivations candidate/ordinary/specialized {}/{}/{}; first/duplicate lowering {:.3}/{:.3} ms",
            population.candidate_raw_instructions,
            population.candidate_raw_cells,
            population.candidate_raw_blocks,
            population.candidate_cleaned_instructions,
            population.candidate_cleaned_cells,
            population.candidate_cleaned_blocks,
            population.candidate_lowered_keys,
            population.ordinary_lowering_derivations,
            population.specialized_lowering_derivations,
            ms(population.first_lower_ns),
            ms(population.duplicate_lower_ns),
        );
        println!(
            "      ownership cost: clone/compact {:.3}/{:.3} ms; clone capacity i/c/b {}/{}/{}; compact capacity i/c/b {}/{}/{}; splice {:.3} ms",
            ms(population.candidate_full_clone_ns),
            ms(population.candidate_compact_ns),
            population.candidate_full_clone_capacity.0,
            population.candidate_full_clone_capacity.1,
            population.candidate_full_clone_capacity.2,
            population.candidate_compact_capacity.0,
            population.candidate_compact_capacity.1,
            population.candidate_compact_capacity.2,
            ms(census.splice_ns),
        );
        println!(
            "      population keys: requested {}, spliced {}, published-never-requested {}, requested-never-spliced {}, requested-not-published {}",
            population.candidate_requested_unique_keys,
            population.candidate_spliced_unique_keys,
            population.candidate_published_never_requested,
            population.candidate_requested_never_spliced,
            population.candidate_requested_not_published,
        );
        if !attribution
            .inline
            .candidate_setup_worker_derivations
            .is_empty()
        {
            println!(
                "        candidate table derivations by worker: {:?}",
                attribution.inline.candidate_setup_worker_derivations
            );
        }
        if !attribution
            .inline
            .candidate_setup_rejection_reasons
            .is_empty()
        {
            println!(
                "        candidate table rejection reasons: {:?}",
                attribution.inline.candidate_setup_rejection_reasons
            );
        }
        println!(
            "      accepted: straight {}, general {}, direct {}, transitive {}, ordinary {}, specialized {}, bounded {}, hoisted {}; clone source i/c/b {}/{}/{} live {}/{}/{} cloned {}/{}/{}",
            census.straight_accepted,
            census.general_accepted,
            census.direct_splices,
            census.transitive_splices,
            attribution.inline.ordinary_splices,
            attribution.inline.specialized_splices,
            attribution.inline.bounded_splices,
            attribution.inline.hoisted_splices,
            census.clone.source_instructions,
            census.clone.source_cells,
            census.clone.source_blocks,
            census.clone.live_instructions,
            census.clone.live_cells,
            census.clone.live_blocks,
            census.clone.cloned_instructions,
            census.clone.cloned_cells,
            census.clone.cloned_blocks,
        );
        println!(
            "      targeted result binding: index {} values/{:.3} ms, bindings {}, users {}, terminators {}, replacements {}, {:.3} ms",
            census.use_index_values,
            ms(census.use_index_build_ns),
            census.targeted_bindings,
            census.targeted_binding_users,
            census.targeted_binding_terminators,
            census.targeted_binding_replacements,
            ms(census.targeted_binding_ns),
        );
        println!(
            "      cleanup load/store by cell producer: loads before/unlinked/spliced-unlinked/added; stores before/unlinked/spliced-unlinked/added"
        );
        for producer in skull_mir::mir::CellProducer::ALL {
            let row = census.load_store.rows[producer as usize];
            if row.is_empty() {
                continue;
            }
            println!(
                "        {:14} loads {}/{}/{}/{}; stores {}/{}/{}/{}",
                producer.label(),
                row.loads_before,
                row.loads_unlinked,
                row.spliced_loads_unlinked,
                row.loads_added,
                row.stores_before,
                row.stores_unlinked,
                row.spliced_stores_unlinked,
                row.stores_added,
            );
        }
        if !census.candidate_worker_derivations.is_empty() {
            println!(
                "      candidate derivations by worker: {:?}",
                census.candidate_worker_derivations
            );
        }
        if !census.rejection_reasons.is_empty() {
            println!(
                "      candidate rejection reasons: {:?}",
                census.rejection_reasons
            );
        }
    }
    if !attribution.inline.top_owners.is_empty() {
        println!("    top inline owners:");
        for owner in &attribution.inline.top_owners {
            println!(
                "      {:28} {} splices, {} copied instructions, {:.3} ms, live {}",
                owner.owner,
                owner.splices,
                owner.copied_instructions,
                owner.splice_ns as f64 / 1_000_000.0,
                owner.live_cloned_entries,
            );
        }
    }
    if !attribution.top_bodies.is_empty() {
        println!("    top lower bodies:");
        for body in &attribution.top_bodies {
            println!(
                "      {:28} {:>7} us, {} MIR instructions",
                body.owner, body.lower_us, body.mir_instructions,
            );
        }
    }
}

/// The machine-readable report: per-iteration means of the same boundary and
/// phase timings the pretty report prints, without the one-run census lines.
fn json_report(input: &str, iters: usize, warmup: usize, stats: &Stats) -> serde_json::Value {
    let per = iters as f64;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / per;
    let phases = |phases: &[(&'static str, Duration)], prefix: &str| -> serde_json::Value {
        phases
            .iter()
            .filter_map(|(name, dur)| {
                name.strip_prefix(prefix)
                    .map(|rest| (rest.to_string(), serde_json::json!(ms(*dur))))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let front = &stats.front;
    let front_phase_sum: Duration = stats.front_phases.iter().map(|(_, d)| *d).sum();
    serde_json::json!({
        "input": input,
        "iters": iters,
        "warmup": warmup,
        "files": stats.files,
        "front": {
            "ok": stats.checked,
            "parse_ms": ms(front.parse),
            "semantic_ms": ms(front.semantic),
            "semantic_call_ms": ms(front.semantic_call),
            "stdlib_assemble_ms": ms(front.stdlib_assemble),
            "teardown_ms": ms(front.semantic_call.saturating_sub(front_phase_sum)),
            "phases": phases(&stats.front_phases, ""),
        },
        "emit": {
            "ok": stats.emitted,
            "workspace_setup_ms": ms(stats.emit.workspace_setup),
            "emit_ms": ms(stats.emit.emit),
            "setup_phases": phases(&stats.emit_phases, "setup: "),
            "join_phases": phases(&stats.emit_phases, "join: "),
            // Per-unit elapsed sums; they overlap under parallel emit, so they
            // exceed `emit_ms` and are comparable only with each other.
            "emit_phases": phases(&stats.emit_phases, ""),
            "failure": stats.failure.as_ref().map(|e| e.to_string()),
        },
    })
}

fn print_report(input: &str, iters: usize, warmup: usize, stats: &Stats) {
    let per = iters as f64;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / per;
    let front = &stats.front;
    let emit = &stats.emit;

    println!("\n{}", format!("=== backend-stats: {} ===", input).bold());
    println!(
        "input: {} user file(s); mean of {} timed iter(s) after {} warmup(s)",
        stats.files, iters, warmup
    );

    println!(
        "\n{}",
        "FRONT END (parse + semantic[lower+check], whole workspace)".bold()
    );
    println!("  parse          : {:>9.3} ms", ms(front.parse));
    println!(
        "  stdlib assemble: {:>9.3} ms  (in semantic; per-run harness cost)",
        ms(front.stdlib_assemble)
    );
    println!("  semantic       : {:>9.3} ms", ms(front.semantic));
    if !stats.front_phases.is_empty() {
        println!("  phases:");
        let mut phase_sum = Duration::ZERO;
        for (name, dur) in &stats.front_phases {
            phase_sum += *dur;
            println!("    {:28} {:>9.3} ms", name, ms(*dur));
        }
        // Caller-measured call captures bone's input-AST teardown at fn return,
        // which no internal phase timer can see: call = phase_sum + teardown.
        let teardown = front.semantic_call.saturating_sub(phase_sum);
        println!(
            "    {:28} {:>9.3} ms  (input-AST drop at return)",
            "teardown",
            ms(teardown)
        );
        println!(
            "    {:28} {:>9.3} ms  (call, incl teardown)",
            "= semantic_pass total",
            ms(front.semantic_call)
        );
    }
    let check = if stats.checked {
        "OK".green()
    } else {
        "FAILED".red()
    };
    println!("  type-check     : {check}");

    println!(
        "\n{}",
        "EMIT (workspace setup + codegen of the checked workspace)".bold()
    );
    println!(
        "  workspace setup: {:>9.3} ms  (comptime/build-var preparation; separate from emit)",
        ms(emit.workspace_setup)
    );
    println!("  emit           : {:>9.3} ms", ms(emit.emit));
    if !stats.emit_phases.is_empty() {
        let setup_sum: Duration = stats
            .emit_phases
            .iter()
            .filter(|(name, _)| name.starts_with("setup: "))
            .map(|(_, d)| *d)
            .sum();
        let emit_phases: Vec<_> = stats
            .emit_phases
            .iter()
            .filter(|(name, _)| !name.starts_with("setup: ") && !name.starts_with("join: "))
            .collect();
        if setup_sum > Duration::ZERO {
            println!("  emit setup (direct wall-clock):");
            for (name, dur) in &stats.emit_phases {
                if name.starts_with("setup: ") {
                    println!("    {:28} {:>9.3} ms", name, ms(*dur));
                }
            }
            // Joined-group children overlap, so the group total already includes their wall time.
            for (name, dur) in &stats.emit_phases {
                if name.starts_with("join: ") {
                    println!("    {:28} {:>9.3} ms  (overlapping child)", name, ms(*dur));
                }
            }
            println!("    {:28} {:>9.3} ms", "= setup total", ms(setup_sum));
        }
        let phase_sum: Duration = emit_phases.iter().map(|(_, d)| *d).sum();
        let emit_body = emit.emit.saturating_sub(setup_sum);
        // Per-unit timers overlap. Estimate their wall shares after subtracting the measured setup time.
        let parallel = phase_sum > emit_body;
        if parallel {
            println!(
                "  emit phases (est. wall-clock share of the {:.3} ms after setup; elapsed sum in parens):",
                ms(emit_body)
            );
            let total = ms(phase_sum);
            let wall = ms(emit_body);
            for (name, dur) in &emit_phases {
                let share = if total > 0.0 {
                    ms(*dur) / total * wall
                } else {
                    0.0
                };
                println!(
                    "    {:28} {:>9.3} ms  (elapsed {:>8.3} ms)",
                    name,
                    share,
                    ms(*dur)
                );
            }
            let ratio = if emit_body.is_zero() {
                0.0
            } else {
                total / wall
            };
            println!("    {:28} {:>8.2}x", "elapsed overlap factor", ratio);
        } else {
            println!("  emit phases:");
            for (name, dur) in &emit_phases {
                println!("    {:28} {:>9.3} ms", name, ms(*dur));
            }
            // Wall-clock around the emit call captures alloc/setup the phase timers
            // don't bracket: overhead = emit remainder - sum(phases).
            let overhead = emit_body.saturating_sub(phase_sum);
            println!(
                "    {:28} {:>9.3} ms  (timer overhead / unbracketed)",
                "overhead",
                ms(overhead)
            );
        }
    }
    print_attribution(&emit.attribution);
    let lanes = &emit.setup_lanes;
    if lanes.cpu_population > 0 || lanes.plans_population > 0 {
        println!(
            "  setup lanes (one timed run): cpu {}, plans {}, shader (beside the fan-out) {}; first={:?}; overlap cpu/plans={}; shared MonoInterner writes/acquires={}, attempts={}, new={}",
            lanes.cpu_population,
            lanes.plans_population,
            lanes.shader_population,
            lanes.first_started_lane,
            lanes.cpu_plans_overlap,
            lanes.mono_write_acqs_during_join,
            lanes.mono_intern_attempts_during_join,
            lanes.mono_intern_new_during_join,
        );
    }
    let graph = &emit.graph_census;
    if graph.expanded > 0 {
        println!(
            "  graph walks (one timed run): {} owners, {} settle rounds",
            graph.expanded, graph.settle_rounds,
        );
    }
    let memo = &stats.abi_plan_stats;
    if memo.requests() > 0 {
        let per_run = |v: u64| v / iters as u64;
        let asked = per_run(memo.requests());
        let hit = per_run(memo.hits());
        let rate = hit as f64 / asked as f64 * 100.0;
        println!("  abi-plan memo  : {asked} asked, {hit} hit per run ({rate:.1}% hit rate)");
        println!(
            "                   owner {}/{} hit, callee {}/{} hit per run",
            per_run(memo.owner_hits),
            per_run(memo.owner_hits + memo.owner_misses),
            per_run(memo.callee_hits),
            per_run(memo.callee_hits + memo.callee_misses),
        );
        println!(
            "                   {} derivations over {} unique keys per run ({} duplicate cold)",
            per_run(memo.stores),
            per_run(memo.unique_keys),
            per_run(memo.duplicate_cold_stores),
        );
    }
    let traffic = &emit.arena_traffic;
    if traffic.rep_final.plans > 0 {
        let (rs, rf) = (&traffic.rep_setup, &traffic.rep_final);
        println!(
            "  rep arena      : {} plans ({} post-setup), one run",
            rf.plans,
            rf.plans - rs.plans,
        );
        let (ms, mf) = (&traffic.mono_setup, &traffic.mono_final);
        println!(
            "  mono interner  : {} write acqs ({} post-setup), {} new types ({} post-setup) of {}, \
             one run",
            mf.write_acqs,
            mf.write_acqs - ms.write_acqs,
            mf.intern_new,
            mf.intern_new - ms.intern_new,
            mf.types,
        );
    }
    let failure = || {
        stats
            .failure
            .as_ref()
            .map_or("missing diagnostic".to_string(), ToString::to_string)
    };
    if stats.emitted {
        println!("  emit           : {}", "OK".green());
    } else if emit.emit_failed {
        println!("  emit           : {} ({})", "FAILED".red(), failure());
    } else if emit.setup_failed {
        println!(
            "  emit           : {} (workspace setup: {})",
            "SKIPPED".red(),
            failure()
        );
    } else {
        println!("  emit           : {} ({})", "SKIPPED".red(), failure());
    }

    let post_parse = front.semantic + emit.workspace_setup + emit.emit;
    println!("\n{}", "TOTAL (parsing excluded)".bold());
    println!("  post-parse     : {:>9.3} ms", ms(post_parse));
    if !front.semantic.is_zero() {
        println!(
            "  emit / semantic: {:.2}x  (codegen vs typecheck)",
            emit.emit.as_secs_f64() / front.semantic.as_secs_f64()
        );
    }
}
