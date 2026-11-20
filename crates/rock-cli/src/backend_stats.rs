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
        "    inline results: {} accepted splices, {} copied instructions; cleanup live before/after {}/{}",
        attribution.inline.accepted_splices,
        attribution.inline.copied_instructions,
        attribution.inline.cleanup_live_before,
        attribution.inline.cleanup_live_after,
    );
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
/// phase timings the pretty report prints, without the one-run attribution lines.
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
