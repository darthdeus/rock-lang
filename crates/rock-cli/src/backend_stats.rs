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
//! per-phase counters (`Workspace::phase_timings`) — see `docs/bone-backend-plan.md`.

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
    /// skull's emit phases summed across timed runs (mean = value / iters), in
    /// run order — same element-wise accumulation as bone's `phases`. `setup:`
    /// entries are direct wall-clock measurements; the remaining entries are
    /// per-unit elapsed timers that overlap under parallel emit.
    emit_phases: Vec<(&'static str, Duration)>,
}

pub fn do_backend_stats(input: &str, iters: usize, warmup: usize) -> Result<()> {
    let iters = iters.max(1);

    // One source load, consumed by both backends so the input is identical. Both
    // type-check the whole set as one workspace (the only way cross-file
    // references resolve); bone supplies the stdlib via `bone_stdlib_modules`.
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
        stats.times.emit += times.emit;
        merge_phases(&mut stats.emit_phases, &times.emit_phases);
    }
    stats.files = sources.files.len();
    stats
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
    let skull_postparse = skull.times.semantic + skull.times.emit;

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
            .filter(|(name, _)| !name.starts_with("setup: "))
            .collect();
        if setup_sum > Duration::ZERO {
            println!("  emit setup (direct wall-clock):");
            for (name, dur) in &skull.emit_phases {
                if name.starts_with("setup: ") {
                    println!("    {:28} {:>9.3} ms", name, ms(*dur));
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
    println!("  post-parse     : {:>9.3} ms", ms(skull_postparse));
    println!(
        "  emit           : {}",
        verdict(skull.ok, "OK", "skipped (bone rejected workspace)")
    );

    // --- A/B (post-parse only) ---
    // bone is the type-check floor (semantic only); skull is that same front
    // half PLUS Lua codegen, so `emit` is the codegen delta bone doesn't pay.
    let bone_ms = ms(bone_postparse);
    let skull_ms = ms(skull_postparse);
    let skull_sem_ms = ms(skull.times.semantic);
    let skull_emit_ms = ms(skull.times.emit);
    println!("\n{}", "A/B (post-parse work; parsing excluded)".bold());
    println!("  bone  : {:>9.3} ms  (semantic only — typecheck floor)", bone_ms);
    println!("  skull : {:>9.3} ms  (semantic + codegen)", skull_ms);
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
         (docs/bone-backend-plan.md)."
            .dimmed()
    );
}
