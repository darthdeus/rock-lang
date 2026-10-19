//! `rock backend-stats <X>` — performance comparison of the two live backend
//! stages on the same input, where `X` is a `.rock` file, a test file, or a
//! stick project directory.
//!
//! Two columns, the second a strict superset of the first in work done:
//!   - **bone**  — `parse` + `passes::semantic_pass` (fused lower + check),
//!     which IS bone's semantic stage. No codegen — the type-check floor.
//!   - **skull** — bone's parse + semantic, THEN `skull::emit_workspace_mir`
//!     (Lua codegen). skull shares bone's front half, so its `emit` phase is
//!     the codegen delta over bone.
//!
//! (The old three-way `rock`/`bone`/`skull` A/B/C is gone — rock was deleted in
//! the rock-deletion arc, `3e204ead6`; bone/skull are the source of truth.)
//!
//! Both consume the SAME loaded source set and type-check it as ONE workspace
//! (the only way cross-file references resolve — a per-file loop would reject
//! every file that names a sibling, and skull's `emit` would never run). Honest
//! asymmetries are reported, not hidden: if bone has a feature gap that rejects
//! the workspace, its semantic time stops at the failure and skull's `emit` is
//! skipped (reads 0); the report prints bone's type-check verdict so a 0 emit is
//! never silent. bone/skull are timed externally at the call boundaries only
//! (parse, semantic_pass, emit); finer bone granularity comes from bone's own
//! per-phase counters (`Workspace::phase_timings`) — see the compiler contract.

use crate::Result;
use crate::bone_backend::{self, BonePhaseTimes};
use crate::skull_backend::{self, SkullPhaseTimes};
use colored::Colorize;
use rock_core::SourceFiles;
use std::time::Duration;

/// Counters + timings from type-checking the whole loaded source set through
/// bone as one workspace. `ok` is whether bone accepted the project. `phases` is
/// bone's own per-phase breakdown (`Workspace::phase_timings`), summed across
/// timed iters in run order.
#[derive(Default)]
struct BoneStats {
    times: BonePhaseTimes,
    files: usize,
    ok: bool,
    phases: Vec<(&'static str, Duration)>,
}

/// Accumulate one run's ordered phase list into `acc`. bone emits the same
/// phases in the same order every run, so we sum element-wise by position.
fn merge_phases(acc: &mut Vec<(&'static str, Duration)>, new: &[(&'static str, Duration)]) {
    if acc.is_empty() {
        acc.extend_from_slice(new);
    } else {
        for (a, n) in acc.iter_mut().zip(new) {
            a.1 += n.1;
        }
    }
}

/// Counters + timings from running skull (bone front-half + Lua codegen) over
/// the whole loaded source set as one workspace. `ok` is whether bone accepted
/// the project AND skull emitted Lua for the entry module.
#[derive(Default)]
struct SkullStats {
    times: SkullPhaseTimes,
    files: usize,
    ok: bool,
    failure: Option<rock_core::CompilerError>,
    setup_failed: bool,
    emit_failed: bool,
    /// skull's emit phases summed across timed runs (mean = value / iters), in
    /// run order — same element-wise accumulation as bone's `phases`. `setup:`
    /// entries are direct wall-clock measurements; the remaining entries are
    /// per-unit elapsed timers that overlap under parallel emit.
    emit_phases: Vec<(&'static str, Duration)>,
    /// ABI-plan memo counters summed across timed runs. Counts, not times, so
    /// they are load-immune and identical run to run.
    abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Shared-arena traffic split from the LAST timed run (counts are stable
    /// run to run modulo worker scheduling, so one run is representative).
    arena_traffic: skull::mir_emit::ArenaTraffic,
    /// Body attribution from the LAST timed run. Durations are intentionally
    /// not summed across runs: the distribution and top-owner rows are one
    /// concrete corpus run, while phase wall shares above are run means.
    attribution: skull::mir_emit::EmitAttribution,
    /// Setup-lane census from the LAST timed run; overlap is an interval fact,
    /// not a ratio inferred from noisy wall-clock totals.
    setup_lanes: skull::mir_emit::SetupLaneAttribution,
    /// Graph operation census from the LAST timed run.
    graph_census: skull::instance_graph::RunStats,
    /// Concrete-plan ownership census from the LAST timed run.
    concrete_stats: skull::concrete_body::ConcreteBuildStats,
}

pub fn do_backend_stats(input: &str, iters: usize, warmup: usize) -> Result<()> {
    let iters = iters.max(1);

    // One source load, consumed by both backends so the input is identical. Both
    // type-check the whole set as one workspace (the only way cross-file
    // references resolve); bone supplies the stdlib via `skull::pipeline::stdlib_modules`.
    let source_spec = crate::common::path_to_source_spec(input);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    if sources.files.is_empty() {
        anyhow::bail!("no Rock source files found at `{}`", input);
    }

    // Real-bench shape: `warmup` untimed runs per backend (warm the allocator,
    // JIT, LazyLock stdlib parse, OS caches) before the `iters` timed runs, so
    // the reported means aren't skewed by first-run cold-start cost.
    let bone = run_bone(&sources, iters, warmup);
    let skull = run_skull(&sources, iters, warmup);

    print_report(input, iters, warmup, &bone, &skull);
    if skull.setup_failed || skull.emit_failed {
        let stage = if skull.setup_failed { "workspace setup" } else { "emit" };
        if let Some(error) = &skull.failure {
            anyhow::bail!("backend-stats skull {stage} failed: {error}");
        }
        anyhow::bail!("backend-stats skull {stage} failed without a diagnostic");
    }
    Ok(())
}

/// Type-check the whole loaded source set through bone as one workspace,
/// `warmup` untimed + `iters` timed runs, accumulating per-stage timing only for
/// the timed runs; `ok` is the final run's verdict.
fn run_bone(sources: &SourceFiles, iters: usize, warmup: usize) -> BoneStats {
    let mut stats = BoneStats::default();
    for i in 0..(warmup + iters) {
        let (res, times) = bone_backend::bone_workspace_timed(sources);
        // `Ok(ws)` only means lowered-and-checked; the verdict must include
        // per-body type errors (`bone_backend::body_errors`), else a
        // project with broken bodies reports as passing.
        stats.ok = res
            .as_ref()
            .is_ok_and(|ws| bone_backend::body_errors(ws).is_empty());
        if i < warmup {
            continue; // untimed warmup
        }
        stats.times.parse += times.parse;
        stats.times.semantic += times.semantic;
        stats.times.stdlib_clone += times.stdlib_clone;
        stats.times.stdlib_extend += times.stdlib_extend;
        stats.times.semantic_call += times.semantic_call;
        // bone's per-phase split lives on the checked Workspace.
        if let Ok(ws) = &res {
            merge_phases(&mut stats.phases, &ws.phase_timings);
        }
    }
    stats.files = sources.files.len();
    stats
}

/// Run skull over the whole loaded source set as one workspace, `warmup`
/// untimed + `iters` timed runs, accumulating per-stage timing (parse + semantic
/// shared with bone, plus skull's emit) only for the timed runs; `ok` is the
/// final run's emit verdict.
fn run_skull(sources: &SourceFiles, iters: usize, warmup: usize) -> SkullStats {
    let mut stats = SkullStats::default();
    for i in 0..(warmup + iters) {
        let (ok, times) = skull_backend::skull_workspace_timed(sources);
        stats.ok = ok;
        if i < warmup {
            continue; // untimed warmup
        }
        stats.times.parse += times.parse;
        stats.times.semantic += times.semantic;
        stats.times.workspace_setup += times.workspace_setup;
        stats.times.emit += times.emit;
        stats.failure = times.failure.clone();
        stats.setup_failed = times.setup_failed;
        stats.emit_failed = times.emit_failed;
        merge_phases(&mut stats.emit_phases, &times.emit_phases);
        stats.abi_plan_stats.merge(&times.abi_plan_stats);
        stats.arena_traffic = times.arena_traffic;
        stats.attribution = times.attribution;
        stats.setup_lanes = times.setup_lanes;
        stats.graph_census = times.graph_census;
        stats.concrete_stats = times.concrete_stats;
    }
    stats.files = sources.files.len();
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
    phase("hoisted-body inline", &attribution.hoisted_inline);
    phase("post-transform validation", &attribution.validation);
    phase("regalloc", &attribution.regalloc);
    phase("stringify", &attribution.stringify);
    println!(
        "    lower facts: concrete_mono {} requests, {} cache hits, {} derivations, {} unique ids, {} shared handles; clones total/miss/insertion/hit/owned {}/{}/{}/{}/{}; explicit owned escapes {}; shared-read/cache/owned-clone self {:.3}/{:.3}/{:.3} ms (timed requests {}); ABI param copied slots {}, shared views {}; MIR inst/cell/block alloc {}/{}/{} realloc {}/{}/{} capacity {}/{}/{}",
        attribution.lower.concrete_mono_requests,
        attribution.lower.concrete_mono_cache_hits,
        attribution.lower.concrete_mono_derivations,
        attribution.lower.concrete_mono_unique_ids,
        attribution.lower.concrete_mono_shared_handle_requests,
        attribution.lower.concrete_mono_cloned_nodes,
        attribution.lower.concrete_mono_interner_miss_cloned_nodes,
        attribution.lower.concrete_mono_cache_insertion_cloned_nodes,
        attribution.lower.concrete_mono_cache_hit_cloned_nodes,
        attribution.lower.concrete_mono_explicit_owned_escape_cloned_nodes,
        attribution.lower.concrete_mono_explicit_owned_escapes,
        attribution.lower.concrete_mono_lookup_ns as f64 / 1_000_000.0,
        attribution.lower.concrete_mono_cache_lookup_ns as f64 / 1_000_000.0,
        attribution.lower.concrete_mono_clone_self_ns as f64 / 1_000_000.0,
        attribution.lower.concrete_mono_timed_requests,
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
    let accounted_ownership_ns = attribution
        .lower
        .concrete_mono_lookup_ns
        .saturating_add(attribution.lower.concrete_mono_clone_self_ns);
    let lower_ns = attribution.lower.elapsed.as_nanos() as u64;
    println!(
        "    lower ownership remainder: {:.3} ms (lower wall {:.3} ms minus timed interner/clone self; overlapping counters are not summed)",
        lower_ns.saturating_sub(accounted_ownership_ns) as f64 / 1_000_000.0,
        lower_ns as f64 / 1_000_000.0,
    );
    println!("    concrete_mono consumers:");
    for consumer in skull_mir::lower::ConcreteMonoConsumer::ALL {
        let stats = attribution.lower.concrete_mono_by_consumer[consumer as usize];
        if stats.requests == 0 {
            continue;
        }
        println!(
            "      {:20} requests {}, cloned nodes {}, owned escapes {}",
            consumer.label(), stats.requests, stats.cloned_nodes, stats.explicit_owned_escapes
        );
    }
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
        if !attribution.inline.candidate_setup_worker_derivations.is_empty() {
            println!(
                "        candidate table derivations by worker: {:?}",
                attribution.inline.candidate_setup_worker_derivations
            );
        }
        if !attribution.inline.candidate_setup_rejection_reasons.is_empty() {
            println!(
                "        candidate table rejection reasons: {:?}",
                attribution.inline.candidate_setup_rejection_reasons
            );
        }
        println!(
            "      accepted: straight {}, general {}, direct {}, transitive {}, ordinary {}, specialized {}, bounded {}, hoisted {}; clone source i/c/b {}/{}/{} live {}/{}/{} cloned {}/{}/{} dead-source {}/{}/{} dead-cloned {}/{}/{} cleanup-roots {}",
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
            census.clone.dead_instructions,
            census.clone.dead_cells,
            census.clone.dead_blocks,
            census.clone.dead_cloned_instructions,
            census.clone.dead_cloned_cells,
            census.clone.dead_cloned_blocks,
            census.clone.cleanup_load_roots,
        );
        println!(
            "      remaps entries/capacity/growth i/c/b {}/{}/{} / {}/{}/{} / {}/{}/{}; arena realloc i/c/b {}/{}/{}; reserve i/c/b {}/{}/{}; args {}/{} dests {}/{}; prefix/tail {}/{}; nested/static {}/{}",
            census.clone.inst_remap_entries,
            census.clone.cell_remap_entries,
            census.clone.block_remap_entries,
            census.clone.inst_remap_capacity,
            census.clone.cell_remap_capacity,
            census.clone.block_remap_capacity,
            census.clone.inst_remap_growth,
            census.clone.cell_remap_growth,
            census.clone.block_remap_growth,
            census.clone.inst_arena_reallocations,
            census.clone.cell_arena_reallocations,
            census.clone.block_arena_reallocations,
            census.clone.caller_reserved_instructions,
            census.clone.caller_reserved_cells,
            census.clone.caller_reserved_blocks,
            census.clone.argument_elements,
            census.clone.argument_stores,
            census.clone.destination_elements,
            census.clone.destination_stores,
            census.clone.block_prefix_copies,
            census.clone.block_tail_copies,
            census.clone.nested_function_imports,
            census.clone.static_imports,
        );
        println!(
            "      substitution: calls {}, instructions {}, terminators {}, replacements {}, {:.3} ms",
            census.substitution_invocations,
            census.substitution_instructions_inspected,
            census.substitution_terminators_inspected,
            census.substitution_replacements,
            ms(census.substitution_ns),
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
        macro_rules! side {
            ($name:literal, $field:ident) => {
                println!(
                    "      side {}: source/examined/copied {}/{}/{}",
                    $name,
                    census.clone.side_tables.$field.source_rows,
                    census.clone.side_tables.$field.examined,
                    census.clone.side_tables.$field.copied,
                );
            };
        }
        side!("spec_bindings", spec_bindings);
        side!("callee_instances", callee_instances);
        side!("abi_plans", abi_plans);
        side!("representation_plans", representation_plans);
        side!("closure_environment_plans", closure_environment_plans);
        side!("raw_bound_trees", raw_bound_trees);
        side!("resolved_bound_trees", resolved_bound_trees);
        side!("function_value_symbols", function_value_symbols);
        side!("instruction_spans", instruction_spans);
        side!("call_sites", call_sites);
        side!("call_site_owners", call_site_owners);
        side!("iterator_headers", iterator_headers);
        if !census.candidate_worker_derivations.is_empty() {
            println!("      candidate derivations by worker: {:?}", census.candidate_worker_derivations);
        }
        if !census.rejection_reasons.is_empty() {
            println!("      candidate rejection reasons: {:?}", census.rejection_reasons);
        }
    }
    if !attribution.inline.top_owners.is_empty() {
        println!("    top inline owners:");
        for owner in &attribution.inline.top_owners {
            println!(
                "      {:28} {} splices, {} copied instructions, {:.3} ms, live {}, dead {}",
                owner.owner,
                owner.splices,
                owner.copied_instructions,
                owner.splice_ns as f64 / 1_000_000.0,
                owner.live_cloned_entries,
                owner.dead_cloned_entries,
            );
        }
    }
    if !attribution.top_bodies.is_empty() {
        println!("    top lower bodies:");
        for body in &attribution.top_bodies {
            println!(
                "      {:28} {:>7} us, {} MIR instructions, concrete_mono {}/{} requests/cloned nodes",
                body.owner,
                body.lower_us,
                body.mir_instructions,
                body.concrete_mono_requests,
                body.concrete_mono_cloned_nodes,
            );
        }
    }
}

fn print_report(input: &str, iters: usize, warmup: usize, bone: &BoneStats, skull: &SkullStats) {
    let per = iters as f64;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / per;
    let verdict = |ok: bool, yes: &str, no: &str| {
        if ok { yes.to_string().green() } else { no.to_string().red() }
    };

    println!("\n{}", format!("=== backend-stats: {} ===", input).bold());
    println!(
        "input: {} user file(s); mean of {} timed iter(s) after {} warmup(s)",
        bone.files, iters, warmup
    );

    // Post-parse = the part that actually differs between the stages. Parsing is
    // shared skull_parser infra, so it's excluded from the A/B.
    let bone_postparse = bone.times.semantic;
    let skull_postparse = skull.times.semantic + skull.times.workspace_setup + skull.times.emit;

    // --- bone (parse + semantic_pass over the whole workspace) ---
    println!("\n{}", "BONE (parse + semantic[lower+check], whole workspace)".bold());
    println!("  stdlib clone   : {:>9.3} ms  (in semantic; per-run harness cost)", ms(bone.times.stdlib_clone));
    println!("  stdlib extend  : {:>9.3} ms  (in semantic)", ms(bone.times.stdlib_extend));
    println!("  parse          : {:>9.3} ms  (excluded from A/B)", ms(bone.times.parse));
    println!("  semantic       : {:>9.3} ms", ms(bone.times.semantic));
    println!("  post-parse     : {:>9.3} ms", ms(bone_postparse));
    // bone's own per-phase split of the semantic column, in run order.
    if !bone.phases.is_empty() {
        println!("  phases:");
        let mut phase_sum = Duration::ZERO;
        for (name, dur) in &bone.phases {
            phase_sum += *dur;
            println!("    {:28} {:>9.3} ms", name, ms(*dur));
        }
        // Caller-measured call captures bone's input-AST teardown at fn return,
        // which no internal phase timer can see: call = phase_sum + teardown.
        let teardown = bone.times.semantic_call.saturating_sub(phase_sum);
        println!("    {:28} {:>9.3} ms  (input-AST drop at return)", "teardown", ms(teardown));
        println!("    {:28} {:>9.3} ms  (call, incl teardown)", "= semantic_pass total", ms(bone.times.semantic_call));
    }
    println!(
        "  type-check     : {}  ({} file workspace)",
        verdict(bone.ok, "OK", "FAILED (bone gap)"),
        bone.files
    );

    // --- skull (bone front-half + Lua codegen) ---
    println!("\n{}", "SKULL (parse + semantic[bone] + emit[codegen], whole workspace)".bold());
    println!("  parse          : {:>9.3} ms  (excluded from A/B)", ms(skull.times.parse));
    println!("  semantic       : {:>9.3} ms  (bone front-half)", ms(skull.times.semantic));
    println!(
        "  workspace setup: {:>9.3} ms  (comptime/build-var preparation; separate from emit)",
        ms(skull.times.workspace_setup)
    );
    println!("  emit           : {:>9.3} ms  (skull codegen delta)", ms(skull.times.emit));
    if !skull.emit_phases.is_empty() {
        let setup_sum: Duration = skull
            .emit_phases
            .iter()
            .filter(|(name, _)| name.starts_with("setup: "))
            .map(|(_, d)| *d)
            .sum();
        let emit_phases: Vec<_> = skull
            .emit_phases
            .iter()
            .filter(|(name, _)| !name.starts_with("setup: ") && !name.starts_with("join: "))
            .collect();
        if setup_sum > Duration::ZERO {
            println!("  emit setup (direct wall-clock):");
            for (name, dur) in &skull.emit_phases {
                if name.starts_with("setup: ") {
                    println!("    {:28} {:>9.3} ms", name, ms(*dur));
                }
            }
            // Joined-group children: they overlap each other (and the group
            // row above already holds the joined wall time), so they are
            // labeled and NEVER added into the setup total.
            for (name, dur) in &skull.emit_phases {
                if name.starts_with("join: ") {
                    println!("    {:28} {:>9.3} ms  (overlapping child)", name, ms(*dur));
                }
            }
            println!("    {:28} {:>9.3} ms", "= setup total", ms(setup_sum));
        }
        let phase_sum: Duration = emit_phases.iter().map(|(_, d)| *d).sum();
        let emit_body = skull.times.emit.saturating_sub(setup_sum);
        // skull emits PIPELINE-parallel: each function runs its whole
        // lower->cleanup->regalloc->stringify pipeline on one rayon worker, so the
        // per-unit phase timers are elapsed-time measurements and their SUM exceeds the wall-clock
        // `emit` line (phases interleave across workers — there is no global
        // per-phase wall span to measure). Setup is already a direct wall-clock
        // measurement, so estimate only the remaining emit time from the
        // per-unit elapsed share. `--features sequential` makes the phase timers
        // non-overlapping wall-clock measurements, so the scaling is a no-op.
        let parallel = phase_sum > emit_body;
        if parallel {
            println!(
                "  emit phases (est. wall-clock share of the {:.3} ms after setup; elapsed sum in parens):",
                ms(emit_body)
            );
            let total = ms(phase_sum);
            let wall = ms(emit_body);
            for (name, dur) in &emit_phases {
                let share = if total > 0.0 { ms(*dur) / total * wall } else { 0.0 };
                println!("    {:28} {:>9.3} ms  (elapsed {:>8.3} ms)", name, share, ms(*dur));
            }
            let ratio = if emit_body.is_zero() { 0.0 } else { total / wall };
            println!("    {:28} {:>8.2}x", "elapsed overlap factor", ratio);
        } else {
            println!("  emit phases:");
            for (name, dur) in &emit_phases {
                println!("    {:28} {:>9.3} ms", name, ms(*dur));
            }
            // Wall-clock around the emit call captures alloc/setup the phase timers
            // don't bracket: overhead = emit remainder - sum(phases).
            let overhead = emit_body.saturating_sub(phase_sum);
            println!("    {:28} {:>9.3} ms  (timer overhead / unbracketed)", "overhead", ms(overhead));
        }
    }
    print_attribution(&skull.attribution);
    let lanes = &skull.setup_lanes;
    if lanes.cpu_population > 0 || lanes.shader_population > 0 || lanes.abi_population > 0 {
        println!(
            "  setup lanes (one timed run): cpu {}, shader {}, ABI {}; first={:?}; overlap cpu/shader={}, ABI/cpu={}, ABI/shader={}; shared MonoInterner writes/acquires={}, attempts={}, new={}",
            lanes.cpu_population,
            lanes.shader_population,
            lanes.abi_population,
            lanes.first_started_lane,
            lanes.cpu_shader_overlap,
            lanes.abi_cpu_overlap,
            lanes.abi_shader_overlap,
            lanes.mono_write_acqs_during_join,
            lanes.mono_intern_attempts_during_join,
            lanes.mono_intern_new_during_join,
        );
    }
    let graph = &skull.graph_census;
    if graph.owner_visits > 0 {
        println!(
            "  graph body facts (one timed run): {} owners, {} expanded; expressions {}, overloads {}, fn-values {}, generated {}, dispatched {}, implicit-iter {}, bound-trees {}, bound-calls {}, iterators {}",
            graph.owner_visits,
            graph.expanded,
            graph.body_exprs_scanned,
            graph.resolved_overload_rows,
            graph.fn_value_rows,
            graph.generated_rows,
            graph.dispatched_rows,
            graph.implicit_iter_rows,
            graph.bound_tree_rows,
            graph.bound_call_rows,
            graph.iterator_rows,
        );
        println!(
            "  graph wavefront (one timed run; census {}): {} frontiers; owners p50/p95/max {}/{}/{}; singleton {}; ordinary/always {}/{}; ready/same-frontier-dependency {}/{}",
            if graph.census_enabled {
                "enabled"
            } else {
                "disabled; set ROCK_GRAPH_CENSUS=1"
            },
            graph.frontier_count,
            graph.frontier_owner_p50,
            graph.frontier_owner_p95,
            graph.frontier_owner_max,
            graph.singleton_frontiers,
            graph.ordinary_owner_visits,
            graph.always_emitted_owner_visits,
            graph.frontier_ready_owners,
            graph.frontier_same_frontier_dependency_owners,
        );
        println!(
            "  graph expansion: owner total/p50/p95/max {} / {} / {} / {} us; mono attempts/new/read/write/timed-mutations {}/{}/{}/{}/{}; interner self-time {:.3} ms",
            graph.expansion_ns_total / 1_000,
            graph.expansion_ns_p50 / 1_000,
            graph.expansion_ns_p95 / 1_000,
            graph.expansion_ns_max / 1_000,
            graph.mono_intern_attempts,
            graph.mono_new_types,
            graph.mono_read_acqs,
            graph.mono_write_acqs,
            graph.mono_timed_mutation_calls,
            graph.mono_interner_self_ns as f64 / 1_000_000.0,
        );
        println!(
            "  graph contributions: reached {}, bound trees {}, edges {}, sites {}, struct-default domains {}, rejections {}, synthesized examined {}, failures {}, always targets {}; index updates subst/trees/erased/always/preds/decls {}/{}/{}/{}/{}/{}; live unique {}/{}/{}/{}/{}/{}",
            graph.contribution_reached,
            graph.contribution_bound_trees,
            graph.contribution_edges,
            graph.contribution_sites,
            graph.contribution_struct_default_domains,
            graph.contribution_rejections,
            graph.contribution_synthesized_examined,
            graph.contribution_failures,
            graph.contribution_always_emitted_targets,
            graph.index_subst_updates,
            graph.index_trees_updates,
            graph.index_erased_updates,
            graph.index_always_emitted_updates,
            graph.index_preds_updates,
            graph.index_decls_updates,
            graph.index_subst_unique,
            graph.index_trees_unique,
            graph.index_erased_unique,
            graph.index_always_emitted_unique,
            graph.index_preds_unique,
            graph.index_decls_unique,
        );
        println!(
            "  graph phases: uninstall {:.3} ms, index {:.3} ms, commit {:.3} ms, derive {:.3} ms, post-pass {:.3} ms",
            graph.uninstall_ns as f64 / 1_000_000.0,
            graph.index_maintenance_ns as f64 / 1_000_000.0,
            graph.index_commit_ns as f64 / 1_000_000.0,
            graph.derive_ns as f64 / 1_000_000.0,
            graph.post_pass_ns as f64 / 1_000_000.0,
        );
    }
    let concrete = &skull.concrete_stats;
    if concrete.plan_attempts > 0 || concrete.always_plan_attempts > 0 {
        println!(
            "  concrete plan facts (one timed run): ordinary plans {}/{} success/fail over {}; always {}/{} over {}; source slots expr/binding {}/{}; callee vecs {} over {} rows; plan maps callees/signatures {}/{}; cloned type tables expr/binding/untyped {}/{}/{}/{}",
            concrete.plan_successes,
            concrete.plan_failures,
            concrete.plan_attempts,
            concrete.always_plan_successes,
            concrete.always_plan_failures,
            concrete.always_plan_attempts,
            concrete.source_expr_slots,
            concrete.source_binding_slots,
            concrete.callee_vec_materializations,
            concrete.callee_vec_rows,
            concrete.plan_callee_map_inserts,
            concrete.plan_callee_signature_inserts,
            concrete.plan_type_expr_clones,
            concrete.plan_type_binding_clones,
            concrete.plan_untyped_expr_clones,
            concrete.plan_untyped_binding_clones,
        );
        println!(
            "    concrete tables (one timed run): type nodes {} examined, {} conversions, {} memo hits, unresolved {}; struct literals {} with {} field conversions; foreign domains {}/{} requests/unique; elapsed ns type/struct/foreign/signature {}/{}/{}/{}",
            concrete.type_nodes_examined,
            concrete.type_node_conversion_attempts,
            concrete.type_node_memo_hits,
            concrete.type_node_unresolved,
            concrete.struct_literals_examined,
            concrete.struct_field_conversion_attempts,
            concrete.foreign_domain_requests,
            concrete.foreign_domain_unique,
            concrete.type_nodes_ns,
            concrete.struct_fields_ns,
            concrete.foreign_domains_ns,
            concrete.signature_ns,
        );
    }
    let memo = &skull.abi_plan_stats;
    if memo.requests() > 0 {
        let per_run = |v: u64| v / iters as u64;
        let asked = per_run(memo.requests());
        let hit = per_run(memo.hits());
        let rate = hit as f64 / asked as f64 * 100.0;
        println!(
            "  abi-plan memo  : {asked} asked, {hit} hit per run ({rate:.1}% hit rate)"
        );
        println!(
            "                   owner {}/{} hit, callee {}/{} hit ({} worker-local) per run",
            per_run(memo.owner_hits),
            per_run(memo.owner_hits + memo.owner_misses),
            per_run(memo.callee_hits),
            per_run(memo.callee_hits + memo.callee_misses),
            per_run(memo.worker_local_hits),
        );
        println!(
            "                   {} derivations over {} unique keys per run \
             ({} duplicate cold), {} param slots cloned by hits",
            per_run(memo.stores),
            per_run(memo.unique_keys),
            per_run(memo.duplicate_cold_stores),
            per_run(memo.cloned_param_slots),
        );
        println!(
            "                   {} shared lookups: {} hits, {} misses per run; {} plan clones \
                 ({} param slots)",
            per_run(memo.shared_lookups),
            per_run(memo.shared_hits),
            per_run(memo.shared_misses),
            per_run(memo.plan_clones),
            per_run(memo.plan_clone_param_slots),
        );
        println!(
            "                   published table: {} hits, {} misses; {} warm keys, {} published \
             keys ({} warm failures) per run",
            per_run(memo.published_hits),
            per_run(memo.published_misses),
            per_run(memo.warm_keys),
            per_run(memo.published_keys),
            per_run(memo.warm_failures),
        );
        println!(
            "                   plan sizes p50/p95/max: {}/{}/{} over {} returned plans per run",
            memo.plan_size_p50(),
            memo.plan_size_p95(),
            memo.plan_size_max,
            per_run(memo.plan_size_samples()),
        );
        if memo.shared_lock_ns > 0 || memo.local_clone_ns > 0 || memo.shared_clone_ns > 0 {
            println!(
                "                   timed (ROCK_LOG=debug): shared lock {:.3} ms, local clone {:.3} ms, \
                 shared clone {:.3} ms, derivation {:.3} ms per run",
                memo.shared_lock_ns as f64 / per / 1_000_000.0,
                memo.local_clone_ns as f64 / per / 1_000_000.0,
                memo.shared_clone_ns as f64 / per / 1_000_000.0,
                memo.derivation_ns as f64 / per / 1_000_000.0,
            );
        }
    }
    let traffic = &skull.arena_traffic;
    if traffic.rep_final.get_calls > 0 {
        let (rs, rf) = (&traffic.rep_setup, &traffic.rep_final);
        println!(
            "  rep arena      : {} gets ({} post-setup, {} worker-local), {} intern attempts \
             ({} post-setup, {} worker-local), {} new plans ({} post-setup), one run",
            rf.get_calls,
            rf.get_calls - rs.get_calls,
            rf.local_get_hits,
            rf.intern_attempts,
            rf.intern_attempts - rs.intern_attempts,
            rf.local_intern_hits,
            rf.intern_new,
            rf.intern_new - rs.intern_new,
        );
        let (ms, mf) = (&traffic.mono_setup, &traffic.mono_final);
        println!(
            "  mono interner  : {} read lock acqs ({} post-setup, {} worker-local reads), \
             {} write acqs ({} post-setup), {} new types ({} post-setup) of {}, one run",
            mf.read_acqs,
            mf.read_acqs - ms.read_acqs,
            mf.local_read_hits,
            mf.write_acqs,
            mf.write_acqs - ms.write_acqs,
            mf.intern_new,
            mf.intern_new - ms.intern_new,
            mf.types,
        );
    }
    println!("  post-parse     : {:>9.3} ms", ms(skull_postparse));
    if skull.ok {
        println!("  emit           : {}", "OK".green());
    } else if skull.emit_failed {
        println!(
            "  emit           : {} ({})",
            "FAILED".red(),
            skull
                .failure
                .as_ref()
                .map_or("missing diagnostic".to_string(), ToString::to_string)
        );
    } else if skull.setup_failed {
        println!(
            "  emit           : {} (workspace setup: {})",
            "SKIPPED".red(),
            skull
                .failure
                .as_ref()
                .map_or("missing diagnostic".to_string(), ToString::to_string)
        );
    } else if let Some(error) = &skull.failure {
        println!("  emit           : {} ({})", "SKIPPED".red(), error);
    } else {
        println!("  emit           : {}", "skipped (bone rejected workspace)".red());
    }

    // --- A/B (post-parse only) ---
    // bone is the type-check floor (semantic only); skull is that same front
    // half PLUS Lua codegen, so `emit` is the codegen delta bone doesn't pay.
    let bone_ms = ms(bone_postparse);
    let skull_ms = ms(skull_postparse);
    let skull_sem_ms = ms(skull.times.semantic);
    let skull_emit_ms = ms(skull.times.emit);
    println!("\n{}", "A/B (post-parse work; parsing excluded)".bold());
    println!("  bone  : {:>9.3} ms  (semantic only — typecheck floor)", bone_ms);
    println!("  skull : {:>9.3} ms  (semantic + workspace setup + codegen)", skull_ms);
    println!("  skull emit (codegen delta over bone) : {:>9.3} ms", skull_emit_ms);
    if skull_sem_ms > 0.0 {
        println!(
            "  skull emit / semantic : {:.2}x  (codegen vs typecheck)",
            skull_emit_ms / skull_sem_ms
        );
    }

    // Caveats — the comparison only reads true when bone accepts the workspace.
    println!(
        "\n{}",
        "note: parsing (shared skull_parser infra) is excluded from the A/B. Both \
         stages type-check the whole source set as one workspace. If bone rejects \
         the workspace (type-check FAILED, a bone feature gap), its semantic time \
         stops at the failure and skull's emit is SKIPPED (reads 0) — the ratios \
         are only meaningful when bone's verdict is OK. Treat the ratios as \
         directional. Finer bone per-pass stats need bone-side counters \
         (the compiler pipeline contract)."
            .dimmed()
    );
}
