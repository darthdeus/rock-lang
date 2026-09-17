//! skull backend integration for `rock-cli` (selected via `--backend skull`).
//!
//! skull is the Claude-managed codegen on top of `bone`'s HIR (see the `skull`
//! crate + `docs/skull-plan.md`). Where `--backend bone` stops at type-checking,
//! `--backend skull` goes one step further: it emits Lua from bone's HIR and
//! (for `test`/`run`) executes it via luajit.
//!
//! A unit "passes" skull today iff: bone accepts it AND skull emits Lua that
//! luajit loads without error. True `@test`-output execution is the next step
//! and lands once bone has a prelude (`print`/`assert`) — until then most files
//! are rejected at the bone stage, exactly like `--backend bone`. The long-term
//! goal is `rock test tests --backend skull` at 100%.

use crate::Result;
use crate::bone_backend::{categorize, collect_rock_files, short_path};
use colored::Colorize;
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Parse `src`, type-check it through bone WITH the stdlib injected (so
/// operators/`print`/`assert` resolve, exactly like `--backend bone`), then emit
/// Lua for the user module via skull. The stdlib module isn't emitted — its
/// symbols are referenced by name (resolved at runtime by a real stdlib
/// prelude, or simply not called under the load-only validity gate).
/// Returns the emitted Lua and the emitter's own `emits_main`, which decides
/// whether the caller appends the entry call. Not recovered from the text: the
/// emitted spelling of a definition is the emitter's business and changes with
/// `ROCK_SKULL_CALLEE_BINDING`.
fn skull_emit(
    file: &str,
    src: &str,
) -> std::result::Result<(String, bool), Vec<CompilerError>> {
    let (lua, _, emits_main) = skull_emit_full(file, src).map_err(EmitFullErr::into_errs)?;
    Ok((lua, emits_main))
}

/// A [`skull_emit_full`] failure. `Discovery` (a malformed `@test` annotation on
/// a file bone ACCEPTED) is split from `Compile` so the expect-compile-failure
/// escape in [`skull_build`] never absorbs it -- `@test(expect_failure = true,
/// returns = 42)` must reject, not count as an expected compile failure.
enum EmitFullErr {
    Compile(Vec<CompilerError>),
    Discovery(CompilerError),
}

impl EmitFullErr {
    fn into_errs(self) -> Vec<CompilerError> {
        match self {
            EmitFullErr::Compile(errs) => errs,
            EmitFullErr::Discovery(e) => vec![e],
        }
    }
}

/// Emit the workspace and also discover the user module's `@test` functions
/// (for execution). Returns `(lua, tests, emits_main)`. `file` is stamped as
/// the source-file id so spans (and `__SITE__`) carry the real path.
fn skull_emit_full(
    file: &str,
    src: &str,
) -> std::result::Result<(String, Vec<skull::SkullTest>, bool), EmitFullErr> {
    let user = skull_parser::parse_module(src, ustr::Ustr::from(file))
        .map_err(|e| EmitFullErr::Compile(vec![e]))?;
    let user_file = user.file;
    let mut ws = crate::bone_backend::check_with_stdlib([user]).map_err(EmitFullErr::Compile)?;
    // Single-file emission resolves from the file's enclosing project
    // (walk-up), matching the JS single-file path -- the same source must
    // agree across backends (crates/rock-cli/tests/js_run_gate.rs).
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(file)).map_err(|e| {
        EmitFullErr::Compile(vec![CompilerError::new_no_location(format!(
            "build var resolution failed: {e:#}"
        ))])
    })?;
    let ws = ws;
    // The build lint pass (`docs/lint-future-plan.md`): soft findings report,
    // `Error` findings refuse the program through the same path a type error
    // takes.
    let lint_errs = crate::lint_pass::run_build_lints_single(&ws, file, src);
    if !lint_errs.is_empty() {
        return Err(EmitFullErr::Compile(lint_errs));
    }
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let emit = skull::mir_emit::emit_workspace_mir(&ws, user_mid)
        .map_err(|e| EmitFullErr::Compile(vec![e]))?;
    let emits_main = emit.emits_main;
    let (lua, skipped) = (emit.lua, emit.skipped);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} stdlib fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }
    let tests = skull::module_tests(&ws, user_mid).map_err(EmitFullErr::Discovery)?;
    Ok((lua, tests, emits_main))
}

/// Per-phase wall-clock for one skull run over a single source. `parse` and
/// `semantic` mirror `BonePhaseTimes` (skull shares bone's front half: same
/// `parse_rock` + `semantic_pass`), so they're directly comparable; `emit` is
/// the codegen skull adds on top (`skull::emit_workspace`). Consumed by
/// `backend-stats` for the bone-vs-skull comparison.
#[derive(Default, Clone)]
pub struct SkullPhaseTimes {
    pub parse: Duration,
    pub semantic: Duration,
    /// Comptime/build-variable preparation after semantic checking and before
    /// code generation. This is setup work, not semantic or emit self-time.
    pub workspace_setup: Duration,
    pub emit: Duration,
    /// The last front-end/setup/emit failure, retained so measurement callers
    /// cannot report a contradictory successful-looking zero population.
    pub failure: Option<CompilerError>,
    pub setup_failed: bool,
    pub emit_failed: bool,
    /// skull's emit broken into coarse stages (lower / cleanup / emit-lua /
    /// scaffold+misc), from `MirEmit::phases`. Empty if bone rejected the
    /// workspace (emit never ran). Summed element-wise across timed runs.
    pub emit_phases: Vec<(&'static str, Duration)>,
    /// skull's ABI-plan memo counters, from `MirEmit::abi_plan_stats`.
    pub abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Shared-arena traffic split at the setup/fanout boundary, from
    /// `MirEmit::arena_traffic`.
    pub arena_traffic: skull::mir_emit::ArenaTraffic,
    /// Body-oriented lowering and transform attribution from `MirEmit`.
    pub attribution: skull::mir_emit::EmitAttribution,
    /// Setup-lane populations and observed execution overlap from `MirEmit`.
    pub setup_lanes: skull::mir_emit::SetupLaneAttribution,
    /// Graph operation census from `MirEmit`.
    pub graph_census: skull::instance_graph::RunStats,
    /// Concrete-plan ownership census from `MirEmit`.
    pub concrete_stats: skull::concrete_body::ConcreteBuildStats,
}

/// Whole-project skull timing: parse all user files, type-check them together
/// with the stdlib as ONE workspace (bone's front half), then emit Lua for the
/// entry module. Returns whether Lua was emitted (the skull "pass" condition,
/// same gate as `skull_emit`) and the per-phase timing. `parse`/`semantic`
/// mirror `bone_workspace_timed`; `emit` is skull's codegen on top. Used by
/// `backend-stats` — a per-file loop would never reach emit because bone rejects
/// any file that references a sibling (so emit would always read ~0).
pub fn skull_workspace_timed(sources: &SourceFiles) -> (bool, SkullPhaseTimes) {
    let mut times = SkullPhaseTimes::default();
    let (user_modules, entry_file, parse) = match crate::bone_backend::parse_user_modules(sources) {
        Ok(v) => v,
        Err(mut errors) => {
            times.failure = errors.drain(..).next();
            return (false, times);
        }
    };
    times.parse = parse;

    let t = Instant::now();
    // Timed path: assemble via the shared helper but keep the `semantic_pass`
    // call explicit here (the verdict is a bool, not a gate, and the timer must
    // bracket exactly this call).
    let mut modules = crate::bone_backend::with_stdlib(user_modules);
    let aliases = match crate::bone_backend::aliases_for_modules(&modules) {
        Ok(aliases) => aliases,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    let mut ws = match bone_semantic::semantic_passes::semantic_pass_with_aliases(&mut modules, &aliases) {
        Ok(ws) => ws,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    times.semantic = t.elapsed();
    let body_errors = crate::bone_backend::body_errors(&ws);
    if let Some(error) = body_errors.into_iter().next() {
        times.failure = Some(error);
        return (false, times);
    }

    // Production emission stamps declared [build.vars] once after the checked
    // workspace exists. Keep that preparation visible as its own timed lane;
    // the emit stopwatch remains codegen-only.
    let t = Instant::now();
    if let Err(error) = crate::comptime::stamp_workspace(
        &mut ws,
        std::path::Path::new(entry_file.as_str()),
    ) {
        times.workspace_setup = t.elapsed();
        times.setup_failed = true;
        times.failure = Some(CompilerError::new_no_location(format!(
            "build var resolution failed: {error:#}"
        )));
        return (false, times);
    }
    times.workspace_setup = t.elapsed();

    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    let t = Instant::now();
    let emit = skull::mir_emit::emit_workspace_mir(&ws, entry_mid);
    times.emit = t.elapsed();
    let emit_ok = match emit {
        Ok(e) => {
            times.emit_phases = e.phases;
            times.abi_plan_stats = e.abi_plan_stats;
            times.arena_traffic = e.arena_traffic;
            times.attribution = e.attribution;
            times.setup_lanes = e.setup_lanes;
            times.graph_census = e.graph_census;
            times.concrete_stats = e.concrete_stats;
            true
        }
        Err(e) => {
            times.failure = Some(e);
            times.emit_failed = true;
            false
        }
    };

    (emit_ok, times)
}

/// `rock iter-codegen <project> --iters N` — build the bone workspace ONCE
/// (parse + semantic), then re-run only skull's `emit_workspace` (the full Lua
/// codegen: lower MIR -> inline -> regalloc -> stringify) N times. The loop body
/// is pure codegen, so a sampling profiler (`samply record -- rock iter-codegen
/// <project> --iters N`) attributes nearly every sample to the codegen stage.
/// Build with `--features sequential` to make skull's emit single-threaded for
/// clean, thread-attribution-free flamegraphs. The counterpart to bone's
/// `iter-compile` (which loops inference) and `iter-parse` (the front end).
pub fn skull_iter_codegen(path: &str, iters: usize) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let iters = iters.max(1);

    // Build the workspace once: parse + bone semantic_pass. Bail clearly if bone
    // can't accept the project (skull emits only what bone lowers).
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    // Warm-up emit outside the timed loop (catch any gap, prime the allocator).
    if let Err(e) = skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
        eprintln!("{}", render_project_err(&sources, &e));
        crate::ablation_report::exit_with_census(1);
    }

    let t = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..iters {
        match skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
            Ok(e) => bytes = e.lua.len(),
            Err(e) => {
                eprintln!("{}", render_project_err(&sources, &e));
                crate::ablation_report::exit_with_census(1);
            }
        }
    }
    let elapsed = t.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let mode = if cfg!(feature = "sequential") { "sequential" } else { "parallel" };
    log::info!(target: "PROFILE",
        "skull iter-codegen {}: {} iters x {} modules ({} KB Lua), {} mode, {:.3}ms total, {:.4}ms/iter",
        path, iters, ws.modules.len(), bytes / 1024, mode,
        elapsed.as_secs_f64() * 1000.0, per);
    Ok(())
}

/// `rock skull-inline-stats <project>` — DIAGNOSTIC: replay skull's function
/// inliner over the whole workspace and report what gets inlined and how much
/// code each splice copies, so we can see whether the emit blow-up comes from a
/// few large callees, one small callee duplicated into many callers, or deep
/// transitive accumulation. Writes a full per-callee table to
/// `skull-inline-stats.txt` (for further analysis) and prints a summary.
///
/// `callee_insts` = reachable inst count of the (cleaned) callee body — the MIR
/// it copies at each site. `total copies` = `callee_insts x times inlined`, a
/// proxy for how much emitted code that callee is responsible for.
pub fn skull_inline_stats(path: &str) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let (mut modules, _entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }

    let r = skull::inline_stats::report(&ws);
    let out = "skull-inline-stats.txt";
    std::fs::write(out, &r.text).map_err(|e| anyhow::anyhow!("writing {out}: {e}"))?;

    println!("{} ({path})", "skull inline stats".bold());
    println!(
        "  {} fns scanned, {} inlined into, {} splices, {} inst-copies",
        r.funcs_scanned, r.callers_inlined, r.total_splices, r.total_copies
    );
    println!("  {} distinct callees", r.distinct_callees);
    if let Some((name, body, times, copies)) = &r.top_callee {
        println!("  most-copied callee: {name} ({body} insts x {times} = {copies} copies)");
    }
    println!("  full report -> {}", out.bold());
    Ok(())
}

pub fn skull_mem2reg_stats(path: &str) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let sources = sources.production_only();
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        eprintln!("Error: build var resolution failed for {path}: {e:#}");
        crate::ablation_report::exit_with_census(1);
    }

    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    let r = match skull::mem2reg_stats::report_in(&ws, entry_mid, Some(path)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{}", render_project_err(&sources, &error));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let out = "skull-mem2reg-stats.txt";
    std::fs::write(out, &r.text).map_err(|e| anyhow::anyhow!("writing {out}: {e}"))?;

    println!("{} ({path})", "skull mem2reg residual census".bold());
    println!(
        "  {} bodies, {} surviving MIR loads ({} cross-block direct)",
        r.bodies_scanned, r.totals.loads, r.totals.cross_block_promotable_loads,
    );
    println!(
        "  {} parameter loads folded by Lua emission; {} residual Store loads",
        r.totals.parameter_promotable_loads, r.totals.store_promotable_loads,
    );
    println!(
        "  {} MIR cell colors; {} residual removable Store colors",
        r.totals.current_cell_slots, r.totals.guaranteed_removable_cell_slots,
    );
    println!("  full report -> {}", out.bold());
    Ok(())
}

/// `rock iter-cook <project> --iters N` — benchmark the front-end COOK
/// (`skull_parser::cook` = macro-expand + per-module desugar), the work bone's
/// `expand_and_desugar` does on every compile. Parses stdlib+project to raw
/// ASTs once, then loops `clone + cook`.
///
/// NOTE: the per-iter `clone` is a HARNESS RESET, not a real compile cost. `cook`
/// mutates the AST in place, so the loop must restore an uncooked copy each
/// iteration. The batch compile path (`semantic_pass` -> `expand_and_desugar` ->
/// `cook`) operates on `&mut [Ast]` in place and clones NOTHING; only the
/// incremental Session clones (`combined_unexpanded`, full-rebuild paths). So
/// read `cook-only` as the real per-compile cost and `clone-only` as harness
/// overhead, not an `expand_and_desugar` `.to_vec()` (no such copy exists today).
pub fn skull_iter_cook(path: &str, iters: usize) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;
    let iters = iters.max(1);

    let (raw, _entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };

    // Warm-up (prime allocator, catch any panic) outside the timed loop.
    {
        let mut c = raw.clone();
        skull_parser::cook(&mut c);
    }

    // Clone-only loop: isolates the harness-reset clone (NOT a batch-compile
    // cost — see fn doc). Skippable via ITER_COOK_NO_CLONE_LOOP=1.
    let mut acc = 0usize;
    let clone_ms = if std::env::var("ITER_COOK_NO_CLONE_LOOP").is_ok() {
        0.0
    } else {
        let tc = Instant::now();
        for _ in 0..iters {
            let c = raw.clone();
            acc = acc.wrapping_add(c.len());
        }
        tc.elapsed().as_secs_f64() * 1000.0 / iters as f64
    };

    // Clone + cook loop: the full `expand_and_desugar` body.
    let t = Instant::now();
    for _ in 0..iters {
        let mut c = raw.clone();
        skull_parser::cook(&mut c);
        acc = acc.wrapping_add(c.len());
    }
    let elapsed = t.elapsed();
    let per = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let _ = acc;
    log::info!(target: "PROFILE",
        "skull iter-cook {}: {} iters x {} modules, {:.4}ms/iter (clone+cook), {:.4}ms/iter clone-only, {:.4}ms/iter cook-only",
        path, iters, raw.len(), per, clone_ms, (per - clone_ms).max(0.0));

    // Split cook into its two sub-phases (macro-expand vs syntax desugar), each
    // timed on a freshly cloned AST so the desugar pass sees post-expand input
    // exactly as `cook` runs it. Clone time is excluded from both sub-totals.
    let (mut exp_ns, mut des_ns) = (0u128, 0u128);
    for _ in 0..iters {
        let mut c = raw.clone();
        let te = Instant::now();
        skull_parser::macro_expand::expand_macros(&mut c);
        exp_ns += te.elapsed().as_nanos();
        let td = Instant::now();
        for ast in c.iter_mut() {
            skull_parser::desugar::desugar(ast);
        }
        des_ns += td.elapsed().as_nanos();
        acc = acc.wrapping_add(c.len());
    }
    let _ = acc;
    let exp_ms = exp_ns as f64 / 1e6 / iters as f64;
    let des_ms = des_ns as f64 / 1e6 / iters as f64;
    log::info!(target: "PROFILE",
        "  cook split: {:.4}ms/iter macro-expand, {:.4}ms/iter desugar",
        exp_ms, des_ms);
    Ok(())
}

/// Assemble a whole *project* directory (a dir with a `stick.toml`) into bone
/// modules: every project + dependency `.rock` file (via the normal compiler's
/// dependency resolver) plus the bone stdlib. Returns the parsed modules and the
/// entry-module file path (`main.rock`, else the first project file); resolve it
/// to a `ModuleId` via `bone_backend::entry_module_id` once the workspace exists.
///
/// Unlike the single-file path, the embedded compiler stdlib is NOT pulled from
/// `discover_sources` — bone supplies its own via `bone_stdlib_modules()`.
pub(crate) fn skull_assemble_project(
    sources: &SourceFiles,
) -> std::result::Result<(Vec<skull_parser::Ast>, ustr::Ustr), Vec<CompilerError>> {
    let (user_modules, entry_file, _parse) = crate::bone_backend::parse_user_modules(sources)?;
    Ok((crate::bone_backend::with_stdlib(user_modules), entry_file))
}

/// A Lua harness appended after the emitted module: it runs each `@test` fn with
/// its `print` AND `io.write` captured (both restored before the markers are
/// written), and writes one `__SKULL__<TAB>PASS|FAIL<TAB>detail` line per test.
/// Capturing `io.write` too is required: tests that emit via `io.write` (raw,
/// no trailing newline) would otherwise bypass the buffer and read as empty.
fn build_test_harness(tests: &[skull::SkullTest]) -> String {
    fn lua_str(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                _ => o.push(c),
            }
        }
        o.push('"');
        o
    }

    let mut h = String::from("\nlocal __skull_T = {\n");
    for t in tests {
        let out = match &t.output {
            Some(s) => lua_str(s),
            None => "nil".to_string(),
        };
        // `f` is the global test fn by its EMITTED symbol; `out` is the expected
        // stdout (or nil). `name` labels the FAIL line and stays the logical
        // DefKey, which is what a human reading the report wants (a package unit
        // runs many tests together, and two modules may declare the same name).
        // `val` marks a `@test(returns = ...)` test: the harness emits a VALUE
        // line carrying the serialized return value; the HOST compares it.
        h.push_str(&format!(
            "  {{ f = {}, out = {}, name = {}, val = {} }},\n",
            t.symbol,
            out,
            lua_str(&t.name),
            t.returns.is_some()
        ));
    }
    h.push_str("}\n");
    h.push_str(
        r#"local function __esc(s) return (s:gsub("\\", "\\\\"):gsub("\n", "\\n"):gsub("\t", "\\t")) end
for _, t in ipairs(__skull_T) do
  local buf = {}
  local rp = print
  local rw = io.write
  -- Model real stdout byte-faithfully: `print` joins args with TAB and adds a
  -- trailing newline; `io.write` appends raw fragments. One accumulator; only
  -- TRAILING whitespace stripped (eats print's `\n` + trailing-space separators
  -- like `{i} ` loops). Leading whitespace is significant so a right-aligned /
  -- space-padded `{x:5}` -> `   42` is testable via @test(output=..).
  print = function(...)
    local p = {}
    for i = 1, select('#', ...) do p[i] = tostring((select(i, ...))) end
    buf[#buf + 1] = table.concat(p, "\t") .. "\n"
  end
  io.write = function(...)
    for i = 1, select('#', ...) do buf[#buf + 1] = tostring((select(i, ...))) end
  end
  local ok, ret = pcall(t.f)
  print = rp
  io.write = rw
  local got = table.concat(buf, ""):gsub("%s+$", "")
  if not ok then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": error: ", __esc(tostring(ret)), "\n")
  elseif t.out ~= nil and got ~= t.out then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": output mismatch: expected <", __esc(t.out), "> got <", __esc(got), ">\n")
  elseif t.val then
    -- %.17g round-trips any f64; booleans via tostring. The host does the
    -- comparison (test_protocol::compare_value), never this harness.
    local s
    if type(ret) == "number" then s = string.format("%.17g", ret) else s = tostring(ret) end
    io.write("\n__SKULL__\tVALUE\t", t.name, "\t", s, "\n")
  else
    io.write("\n__SKULL__\tPASS\t\n")
  end
end
"#,
    );
    h
}

/// Render single-file errors against `src`. `skull_emit_full` parses with the
/// real `file` path as the span file id, so the source must be registered under
/// that same id -- registering under a placeholder makes every span lookup fail
/// ("Unable to fetch source") and silently drops the snippet + labels.
pub(crate) fn render_file_errs(file: &str, src: &str, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles { files: vec![] };
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    crate::bone_backend::render_errs(&sf, errs)
}

/// `map_err` for the `skull::user_module_id` lookup — the same message at every
/// call site.
fn no_user_module(e: CompilerError) -> anyhow::Error {
    anyhow::anyhow!("no user module: {}", e.message)
}

/// A skull failure: a short bucketed reason + the full rendered detail.
#[derive(Debug)]
struct SkullFailure {
    reason: String,
    rendered: String,
}

/// Outcome of building + running one file. `Pass` means it emitted valid Lua
/// AND every `@test` passed (or it had none). `TestFail` means it emitted valid
/// Lua but some `@test` produced the wrong output or errored — these still count
/// toward "emit valid Lua" but not toward execution. Both variants carry the
/// number of individual `@test` functions involved so the runner can report
/// per-`@test` totals, not just per-file ones.
#[derive(Debug)]
enum SkullOutcome {
    /// All `@test`s passed (or the file had none). `tests` = number that ran
    /// green (0 for a load-only file with no `@test`s).
    Pass { tests: usize },
    /// Some `@test`s failed. `passed` ran green; `fails` holds the rest's detail.
    TestFail { passed: usize, fails: Vec<String> },
}

/// Run a source string through the full skull pipeline (bone type-check + skull
/// codegen), confirm the emitted Lua loads, and execute its `@test` functions
/// against the real (emitted) stdlib. `Err` = emit/load failure; `Ok` carries
/// the pass/test-fail outcome.
fn skull_build(file: &str, src: &str) -> std::result::Result<SkullOutcome, SkullFailure> {
    // bone type-check + skull emit. Errors here are bone's (most files) or a
    // skull codegen gap; both carry a `CompilerError`.
    let (lua, tests, _emits_main) = match skull_emit_full(file, src) {
        Ok(v) => v,
        Err(EmitFullErr::Compile(errs)) => {
            // A file whose `@test`s ALL expect failure is supposed to fail to
            // compile — a bone rejection here is the expected (passing) outcome.
            // (Mirrors the real rock runner's compile-Err branch.)
            if let Some(tests) = all_tests_expect_compile_failure(file, src) {
                return Ok(SkullOutcome::Pass { tests });
            }
            return Err(SkullFailure {
                reason: categorize(&errs[0].message),
                rendered: render_file_errs(file, src, &errs),
            });
        }
        Err(EmitFullErr::Discovery(e)) => {
            return Err(SkullFailure {
                reason: categorize(&e.message),
                rendered: render_file_errs(file, src, &[e]),
            });
        }
    };

    let lua = match configured_stone_dir(
        std::path::Path::new(file)
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    ) {
        Ok(Some(stone_dir)) => prepend_stone_root(lua, &stone_dir),
        Ok(None) => lua,
        Err(error) => {
            return Err(SkullFailure {
                reason: "Stone root resolution failed".to_string(),
                rendered: error.to_string(),
            })
        }
    };
    run_emitted_tests(&lua, &tests)
}

/// `Some(count)` when `file` declares `@test` fns and EVERY one carries
/// `expect_failure`/`@ignore` -- a compile rejection is then the expected
/// (passing) outcome and each counts as a passing test. Shared by the luajit
/// and JS executors (both bone-type-check first, so the rule is identical).
pub(crate) fn all_tests_expect_compile_failure(file: &str, src: &str) -> Option<usize> {
    let mut sf = SourceFiles { files: vec![] };
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    let test_fns = crate::test::find_test_functions_from_sources(&sf);
    if !test_fns.is_empty() && test_fns.iter().all(|t| t.expect_failure || t.ignored) {
        Some(test_fns.len())
    } else {
        None
    }
}

/// Given emitted workspace Lua and the `@test` fns discovered in it, run them
/// via the harness (or the load-only validity gate when there are none) and
/// classify the outcome. Shared by the per-file path ([`skull_build`]) and the
/// whole-package path ([`skull_build_package`]).
fn run_emitted_tests(
    lua: &str,
    tests: &[skull::SkullTest],
) -> std::result::Result<SkullOutcome, SkullFailure> {
    let timeout = Some(std::time::Duration::from_secs(15));

    // No @test functions: fall back to the load-only validity gate.
    if tests.is_empty() {
        match rock_host::lua_runner::run_lua_code_with_timeout(lua, timeout) {
            Ok(out) if out.success => return Ok(SkullOutcome::Pass { tests: 0 }),
            Ok(out) => {
                return Err(SkullFailure {
                    reason: "invalid lua emitted (skull bug)".to_string(),
                    rendered: format!(
                        "{}\n\n--- emitted lua (truncated) ---\n{}",
                        out.combined(),
                        truncate(lua)
                    ),
                });
            }
            Err(e) => {
                return Err(SkullFailure {
                    reason: "invalid lua emitted (skull bug)".to_string(),
                    rendered: e.message,
                });
            }
        }
    }

    // Run the @tests: emit + harness, capture each test's PASS/FAIL line.
    let program = format!("{lua}{}", build_test_harness(tests));
    // The harness `pcall`s each test, so an ordinary assertion failure becomes a
    // FAIL marker and the process still exits 0. A non-zero status therefore
    // means the run DIED -- `os.exit`, a C-level abort, an OOM -- and every test
    // after that point never ran. Keep the output either way: a truncated run
    // still has to say what it managed to report.
    let (stdout, died) = match rock_host::lua_runner::run_lua_code_with_timeout(&program, timeout) {
        Ok(run) => (run.combined(), !run.success),
        Err(e) => {
            return Err(SkullFailure {
                reason: "test run errored (skull bug)".to_string(),
                rendered: e.message,
            });
        }
    };

    // No result lines at all => the module didn't even load (a skull bug),
    // not a test failure.
    let Some(markers) = crate::test_protocol::parse_skull_markers(&stdout) else {
        return Err(SkullFailure {
            reason: "invalid lua emitted (skull bug)".to_string(),
            rendered: format!("{stdout}\n\n--- emitted lua (truncated) ---\n{}", truncate(&program)),
        });
    };

    // VALUE lines: compare the serialized return value HOST-side against the
    // pinned `@test(returns = ...)` literal.
    let mut passed = markers.passed;
    let mut fails = markers.fails;
    let expects: HashMap<&str, &skull::ValueExpect> = tests
        .iter()
        .filter_map(|t| t.returns.as_ref().map(|v| (t.name.as_str(), v)))
        .collect();
    for (name, val) in &markers.values {
        let Some(expect) = expects.get(name.as_str()) else {
            return Err(SkullFailure {
                reason: "harness invariant breach (skull bug)".to_string(),
                rendered: format!("VALUE line for `{name}` without a returns expectation"),
            });
        };
        match crate::test_protocol::compare_value(expect, val, crate::test_protocol::FLOAT_REL_TOL)
        {
            Ok(()) => passed += 1,
            Err(detail) => fails.push(format!("{name}: {detail}")),
        }
    }

    // Every declared test owes exactly one marker. Counting only the markers
    // that ARRIVED makes a truncated run self-consistent -- three tests, one
    // marker, `1/1 passed` -- so the tests that never ran leave the total
    // instead of failing it. This is the only signal for a premature
    // `os.exit(0)`, which exits success and emits no marker at all.
    // A PASS marker carries no test name, so this counts rather than names them.
    let reported = passed + fails.len();
    if reported < tests.len() {
        fails.push(format!(
            "the run stopped early: {} of {} @tests never reported{}",
            tests.len() - reported,
            tests.len(),
            if died { "; the process died" } else { "; the process exited 0" }
        ));
    } else if died {
        fails.push(
            "every @test reported, but the harness process exited non-zero -- \
             something after the tests failed"
                .to_string(),
        );
    }

    if fails.is_empty() {
        Ok(SkullOutcome::Pass { tests: passed })
    } else {
        Ok(SkullOutcome::TestFail { passed, fails })
    }
}

/// A directory that is a `stick.toml` package -- the unit `rock test` builds as
/// ONE whole workspace (all its files + deps + its own `tests/`) rather than
/// file-by-file. A `type=lib` split across files can only be tested this way:
/// per-file, a file referencing a sibling's symbol fails to compile. Root
/// `tests/` (no stick.toml, hundreds of independent colliding programs) is NOT
/// a package and stays on the per-file path.
fn is_test_package_dir(path: &str) -> bool {
    let p = std::path::Path::new(path);
    p.is_dir() && p.join("stick.toml").is_file()
}

pub(crate) fn is_package_tests_dir(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let name = p.file_name().and_then(|n| n.to_str());
        if name == Some("tests") || name == Some("tests/") {
            let parent = p.parent().unwrap_or(std::path::Path::new(""));
            let candidate = if parent.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                parent
            };
            if candidate.join("stick.toml").is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Assemble + type-check + emit a whole package (tests INCLUDED), then run its
/// `@test`s. The package analogue of [`skull_build`]. Cross-file references
/// resolve because every file is in one workspace.
fn skull_build_package(dir: &str) -> std::result::Result<SkullOutcome, SkullFailure> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec).map_err(|e| SkullFailure {
        reason: "cannot load project".to_string(),
        rendered: e.to_string(),
    })?;
    // Tests INCLUDED (no `production_only`): the point is to compile the
    // package's own tests/ WITH it, unlike the game-build compile path.
    let (mut modules, entry_file) = skull_assemble_project(&sources).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
    })?;
    let mut ws = crate::bone_backend::check_modules(&mut modules).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
    })?;
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir)).map_err(|e| {
        SkullFailure {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {dir}: {e:#}"),
        }
    })?;
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    let lua = match skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
        Ok(e) => e.lua,
        Err(e) => {
            return Err(SkullFailure {
                reason: categorize(&e.message),
                rendered: render_project_err(&sources, &e),
            });
        }
    };
    let tests = collect_package_tests(&ws, dir).map_err(|e| SkullFailure {
        reason: categorize(&e.message),
        rendered: render_project_err(&sources, &e),
    })?;
    let lua = prepend_package_preamble(lua, dir)?;
    run_emitted_tests(&lua, &tests)
}

/// The preamble a package's `@test`s run under.
///
/// A GAME package gets the game preamble -- the same `__ROCK_GAME_SOURCE_DIR`,
/// asset overlays, stone root and renderer the game itself boots with -- and its
/// engine cdylib staged, so a `@test` resolves an `asset_path` to the file the
/// game would open and can load the engine library when it asks for a headless
/// capability. Without the source dir, `asset_path("assets/x")` answered
/// `"/assets/x"` here, and `stone_load_font` silently fell back to its embedded
/// face: a measurement that is plausible, nonzero and wrong.
///
/// Anything else keeps the stone root alone. A lib or a test-only package has no
/// game directory to point at and no engine to stage.
fn prepend_package_preamble(lua: String, dir: &str) -> std::result::Result<String, SkullFailure> {
    let path = std::path::Path::new(dir);
    if rock_host::project::is_game_project(path).unwrap_or(false) {
        // Core libstone only, not `build_engine_libs`: the test process loads
        // the engine library for CPU-side capabilities and never asks for a
        // game's optional cdylibs (video, wgpu, agent), so building them here
        // would be time spent on libraries nothing in this process opens.
        // A no-op in release layout, where the prebuilt libs are already in
        // place.
        rock_host::engine_build::build_libstone().map_err(|error| SkullFailure {
            reason: "engine library staging failed".to_string(),
            rendered: format!(
                "`rock test {dir}` could not stage libstone for the test process: {error:#}"
            ),
        })?;
        let preamble =
            skull_game_preamble_for(dir, skull::callee_binding_mode()).map_err(|error| {
                SkullFailure {
                    reason: "game preamble resolution failed".to_string(),
                    rendered: error.to_string(),
                }
            })?;
        return Ok(format!("{preamble}{lua}"));
    }
    match configured_stone_dir(path) {
        Ok(Some(stone_dir)) => Ok(prepend_stone_root(lua, &stone_dir)),
        Ok(None) => Ok(lua),
        Err(error) => Err(SkullFailure {
            reason: "Stone root resolution failed".to_string(),
            rendered: error.to_string(),
        }),
    }
}

/// The `@test` fns defined in the package rooted at `dir` -- its own source
/// files and its `tests/`, but NOT stdlib or dependency packages (which live at
/// other canonical paths). Module file paths are canonicalized absolute, so a
/// prefix match against the canonicalized `dir` selects exactly this package.
fn collect_package_tests(
    ws: &bone_semantic::inference::Workspace,
    dir: &str,
) -> std::result::Result<Vec<skull::SkullTest>, CompilerError> {
    Ok(collect_package_test_fns(ws, dir)?.into_iter().map(|(_, t)| t).collect())
}

/// [`collect_package_tests`] carrying each test's `FuncRef` (the JS executor
/// lowers/emits per test fn).
pub(crate) fn collect_package_test_fns(
    ws: &bone_semantic::inference::Workspace,
    dir: &str,
) -> std::result::Result<Vec<(skull_mir::mir::FuncRef, skull::SkullTest)>, CompilerError> {
    use bone_semantic::prelude::ModuleId;
    let root = std::path::Path::new(dir)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(dir));
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        if m.id == ModuleId::builtin() {
            continue;
        }
        let under = std::path::Path::new(m.file.as_str())
            .canonicalize()
            .map(|c| c.starts_with(&root))
            .unwrap_or(false);
        if under {
            out.extend(
                skull::module_tests_with_ids(ws, m.id)?
                    .into_iter()
                    .map(|(fid, t)| (skull_mir::mir::FuncRef { module: m.id, func: fid }, t)),
            );
        }
    }
    Ok(out)
}

/// Keep emitted-Lua dumps in error reports manageable.
fn truncate(s: &str) -> String {
    if s.len() > 4000 {
        format!("{}\n... ({} more bytes)", &s[..4000], s.len() - 4000)
    } else {
        s.to_string()
    }
}

/// Parse the `--profile` value for the `rock shader` entry forms -- the only
/// command that selects a profile (in-game source is emitted once and adapted
/// to the live GL context by the engine). No fallback: an unknown value rejects
/// listing the valid ones.
pub(crate) fn parse_glsl_profile(s: Option<&str>) -> Result<skull_codegen_glsl::GlslProfile> {
    use skull_codegen_glsl::GlslProfile;
    match s {
        None | Some("330core") => Ok(GlslProfile::Core330),
        Some("300es") => Ok(GlslProfile::Es300),
        Some(other) => {
            anyhow::bail!("rock shader: unknown --profile `{other}` (valid: 330core, 300es)")
        }
    }
}

fn emit_shader_program_with_view(
    ws: &bone_semantic::inference::Workspace,
    program: skull::shader_artifact::ShaderProgram,
    profile: skull_codegen_glsl::GlslProfile,
) -> std::result::Result<skull_codegen_glsl::ShaderEmission, skull_parser::CompilerError> {
    let roots: Vec<skull_mir::mir::FuncRef> = program
        .vert
        .into_iter()
        .chain(std::iter::once(program.frag))
        .collect();
    let graph = skull::instance_graph::build(ws, roots, false);
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(graph.interner()));
    let programs = [program];
    let view = skull::shader_artifact::realize_shader_with_context(
        ws,
        &graph,
        &skull::shader_artifact::ShaderArtifactRequest {
            spec: skull::target_projection::TargetSpec {
                shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(profile)),
                ..skull::target_projection::LUAJIT_DESKTOP
            },
            programs: &programs,
        },
        &shader_scx,
    )
    .ok_or_else(|| skull_parser::CompilerError::new_no_location("GLSL shader view is unavailable".to_string()))?;
    let program_view = view
        .program_views
        .first()
        .ok_or_else(|| skull_parser::CompilerError::new_no_location("GLSL shader view has no program".to_string()))?;
    match program.vert {
        Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
            ws,
            &shader_scx,
            program_view,
            profile,
        ),
        None => skull_codegen_glsl::emit_shader_entry_with_view(
            ws,
            &shader_scx,
            program_view,
            profile,
        ),
    }
}

/// `rock shader <file.rock> <fn> [fn...] [--output <file.glsl>]` -- GLSL emission.
/// Three forms:
///   - fn library (route B): plain roots, no `#version` line -- `--profile`
///     rejects here (nothing for it to select).
///   - single `@shader(fragment)` root (A1 fullscreen) and
///     `<vert_fn> <frag_fn>` (A2 pipeline, selected when a root is
///     `@shader(vertex)`): both take `[--profile 330core|300es]`.
/// A profile violation renders spanned against the source and exits non-zero.
pub fn skull_shader(files: &[String], output: Option<String>, profile: Option<String>) -> Result<()> {
    let Some((path, roots)) = files.split_first() else {
        anyhow::bail!("rock shader: usage: rock shader <file.rock> <fn> [fn...] --output <file.glsl>");
    };
    if roots.is_empty() {
        anyhow::bail!("rock shader: need at least one root fn name");
    }
    if let Some(out) = output.as_deref() {
        invalidate_shader_output(path, out)?;
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SHADER", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(path.as_str())) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}", render_file_errs(path, &src, &[e]));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user_file = user.file;
    let ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let roots: Vec<String> = roots.to_vec();

    // Pipeline form: any root is a `@shader(vertex)` entry.
    let is_pipeline = roots
        .iter()
        .any(|r| skull_codegen_glsl::is_vertex_shader_root(&ws, user_mid, r));

    let glsl = if is_pipeline {
        if roots.len() != 2 {
            anyhow::bail!("rock shader: the vertex+fragment pipeline form takes exactly two roots: <vert_fn> <frag_fn>");
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let vert = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        };
        let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[1]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        };
        match emit_shader_program_with_view(
            &ws,
            skull::shader_artifact::ShaderProgram { vert: Some(vert), frag },
            prof,
        )
        .map(|emission| emission.source)
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        }
    } else {
        // The route-B fn library emits no `#version` line, so a profile cannot
        // apply to it; the single-root fullscreen entry takes one like the
        // pipeline form does.
        let is_entry = roots.len() == 1
            && skull_codegen_glsl::is_fragment_shader_root(&ws, user_mid, &roots[0]);
        if profile.is_some() && !is_entry {
            anyhow::bail!("rock shader: --profile applies to shader entries (pipeline or @shader(fragment) fullscreen), not the route-B fn library (it emits no #version line)");
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let glsl = if is_entry {
            let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
                Ok(root) => root,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            emit_shader_program_with_view(
                &ws,
                skull::shader_artifact::ShaderProgram { vert: None, frag },
                prof,
            )
            .map(|emission| emission.source)
        } else {
            let root_refs = roots
                .iter()
                .map(|root| skull_codegen_glsl::resolve_root(&ws, user_mid, root))
                .collect::<std::result::Result<Vec<_>, _>>();
            let root_refs = match root_refs {
                Ok(root_refs) => root_refs,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            let session = match skull_codegen_glsl::shader_library_session(&ws, root_refs) {
                Ok(session) => session,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::ablation_report::exit_with_census(1);
                }
            };
            skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        };
        match glsl {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        }
    };

    match output {
        Some(out) => {
            // Keep the fn-library header text stable: utils_math.glsl byte-identity.
            let mode = if glsl.starts_with("#version") { "shader entry" } else { "route B" };
            let profile_flag = profile.as_deref().map(|p| format!(" --profile {p}")).unwrap_or_default();
            let header = format!(
                "// GENERATED by skull-codegen-glsl ({mode}) -- DO NOT EDIT.\n\
                 // Regenerate: rock shader {} {}{} --output {}\n\n",
                path,
                roots.join(" "),
                profile_flag,
                out
            );
            // A `#version` output puts the header AFTER the version line: the ES
            // profile requires `#version` to be the very first token (a leading
            // comment is a hard error), and the engine loader keeps the header
            // in-source, so a header-before-#version breaks the mobile profile.
            let file = if let Some(rest) = glsl.strip_prefix("#version") {
                let nl = rest.find('\n').map(|p| p + 1).unwrap_or(rest.len());
                let (version_tail, body) = rest.split_at(nl);
                let body = body.strip_prefix('\n').unwrap_or(body);
                format!("#version{version_tail}{header}{body}")
            } else {
                format!("{header}{glsl}")
            };
            rock_host::project::write_atomic(&out, &file)?;
            log::info!(target: "SHADER", "{} [{}] -> {}", path, roots.join(", "), out);
        }
        None => print!("{glsl}"),
    }
    Ok(())
}

fn invalidate_shader_output(input: &str, output: &str) -> Result<()> {
    let input = std::path::Path::new(input);
    let output = std::path::Path::new(output);
    let aliases = input == output
        || match (fs_err::canonicalize(input), fs_err::canonicalize(output)) {
            (Ok(input), Ok(output)) => input == output,
            _ => false,
        };
    if aliases {
        anyhow::bail!("rock shader: --output must not refer to the input file");
    }
    match fs_err::remove_file(output) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn skull_compile_single(path: &str, output: Option<String>) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_compile_project(path, output);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    match skull_emit(path, &src) {
        Ok((lua, _emits_main)) => {
            match output {
                Some(out) => {
                    rock_host::project::write_atomic(&out, &lua)?;
                    log::info!(target: "SKULL", "{} -> {}", path, out);
                }
                None => {
                    // Result output (the compiled Lua), not logging — pipeable.
                    print!("{}", lua);
                }
            }
            Ok(())
        }
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// `rock watch --backend skull` — live recompile loop: on each change,
/// re-discover sources, bone-check, and emit the full `.lua` via the new
/// (cell-model) path to `output` (default `tmp/main.lua`). A parse/type error
/// renders and serves the previous good output; the loop keeps running. (The old
/// IncrementalEmitter was retired with the old IR; a batch re-emit is fast —
/// a game project assembles in a few ms.)
pub fn skull_watch(path: &str, output: Option<String>) -> Result<()> {
    let out_file = output
        .map(std::path::PathBuf::from)
        .unwrap_or_else(rock_host::project::default_main_lua_path);
    if let Some(parent) = out_file.parent() {
        if !parent.exists() {
            fs_err::create_dir_all(parent)?;
        }
    }

    // Live recompile backed by bone's incremental `Session` (warm stdlib,
    // per-body re-inference) + skull's per-body emit cache: a body edit re-emits
    // only the re-inferred chunks (+ their inline-dependent callers), reusing the
    // rest. An interface/structural change (signature, add/remove file) shifts
    // `Idx<Function>`, so the cache is rebuilt with a full emit. See
    // `docs/skull-incremental-emit-plan.md`.
    let mut session: Option<bone_incremental::session::Session> = None;
    let mut cache: Option<skull::mir_emit::SkullEmitCache> = None;

    let build = |label: &str,
                 session: &mut Option<bone_incremental::session::Session>,
                 cache: &mut Option<skull::mir_emit::SkullEmitCache>| {
        let source_spec = crate::common::path_to_source_spec(path);
        let sources = match crate::unified_compile::discover_sources(&source_spec) {
            Ok(s) => s,
            Err(e) => {
                log::error!(target: "WATCH", "skull: source discovery failed: {e}");
                return;
            }
        };
        // Game build: drop test-only files (tests/ dirs) so a broken sim/test
        // can't break the watched game output -- parity with the rock backend's
        // is_game gate. See `docs/sim-layering.md`.
        let path_ref = std::path::Path::new(path);
        let sources = if path_ref.is_dir()
            && rock_host::project::is_game_project(path_ref).unwrap_or(false)
        {
            sources.production_only()
        } else {
            sources
        };
        let files: Vec<(String, String)> = sources
            .files
            .iter()
            .map(|f| {
                let f = f.borrow();
                (f.file.to_id_path(), f.contents.clone())
            })
            .collect();

        let t = Instant::now();
        let mut reinferred: Vec<(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)> = Vec::new();
        let mut full = false; // interface/structural change -> rebuild cache
        let mut update_failed = false;

        match session {
            None => match crate::bone_backend::bone_session_for_emission(
                files.clone(),
                std::path::Path::new(path),
            ) {
                Ok(s) => {
                    *session = Some(s);
                    full = true; // first build: populate the cache
                }
                Err(e) => {
                    eprintln!("{}", e.render(&sources));
                    log::error!(target: "WATCH", "skull {label}: session init failed");
                    return;
                }
            },
            Some(s) => {
                for (file_path, text) in &files {
                    match s.update_file(file_path, text) {
                        Ok(result) => {
                            if matches!(result.tier, bone_incremental::session::UpdateTier::Interface | bone_incremental::session::UpdateTier::FullUser) {
                                full = true;
                            }
                            reinferred.extend(result.reinferred);
                        }
                        Err(e) => {
                            update_failed = true;
                            eprintln!("{}", e.render(&sources));
                        }
                    }
                }
                let gone: Vec<String> = s
                    .tracked_paths()
                    .into_iter()
                    .filter(|p| !files.iter().any(|(fp, _)| fp == p))
                    .map(|p| p.to_string())
                    .collect();
                for file_path in gone {
                    full = true; // removed file shifts module ids
                    if let Err(e) = s.remove_file(&file_path) {
                        update_failed = true;
                        eprintln!("{}", e.render(&sources));
                    }
                }
            }
        }
        let check_ms = t.elapsed().as_secs_f64() * 1000.0;

        let Some(s) = session else { return };
        // A broken edit still COMMITS its partial state to the session (the parse
        // is total now), so the incremental cache -- which reflects the last good
        // emit -- can no longer be diffed against it. Drop the cache on any error
        // so the next clean recompile does a correct full rebuild rather than an
        // incremental emit across the broken intermediate. The reload itself is
        // skipped (last good output keeps running) until the code is clean.
        if update_failed {
            *cache = None;
            log::error!(target: "WATCH", "skull {label}: update failed in {check_ms:.1}ms - serving last good output");
            return;
        }
        let errs = s.diagnostics();
        if !errs.is_empty() {
            *cache = None;
            for e in &errs {
                eprintln!("{}", e.render(&sources));
            }
            log::error!(target: "WATCH", "skull {label}: {} diagnostic(s) in {check_ms:.1}ms - serving last good output", errs.len());
            return;
        }

        // A recompile is a build, so the lint pass runs here too; an `Error`
        // finding refuses the reload the same way a type error does.
        let lint_errs = crate::lint_pass::run_build_lints(
            s.workspace(),
            &sources,
            std::path::Path::new(path),
            crate::lint_pass::Scope::WholeProject,
        );
        if !lint_errs.is_empty() {
            *cache = None;
            for e in &lint_errs {
                eprintln!("{}", render_project_err(&sources, e));
            }
            log::error!(target: "WATCH", "skull {label}: {} lint error(s) - serving last good output", lint_errs.len());
            return;
        }

        let ws = s.workspace();
        let Ok(mid) = skull::user_module_id(ws) else {
            log::error!(target: "WATCH", "skull {label}: no user module");
            return;
        };
        let te = Instant::now();
        let emit = if full || cache.is_none() {
            match skull::mir_emit::emit_full(ws, mid) {
                Ok((e, c)) => {
                    *cache = Some(c);
                    e
                }
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: emit failed - serving last good output");
                    return;
                }
            }
        } else {
            match skull::mir_emit::emit_incremental(ws, cache.as_mut().unwrap(), &reinferred) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: incremental emit failed - serving last good output");
                    return;
                }
            }
        };
        let emit_ms = te.elapsed().as_secs_f64() * 1000.0;
        let mode = if full || reinferred.is_empty() { "full" } else { "incremental" };
        match rock_host::project::write_atomic(&out_file, &emit.lua) {
            Ok(()) => log::info!(target: "WATCH",
                "skull {label}: {} files -> {} ({mode}, {} re-inferred; check {check_ms:.1}ms, emit {emit_ms:.1}ms)",
                sources.files.len(), out_file.display(), reinferred.len()),
            Err(err) => log::error!(target: "WATCH", "skull {label}: write failed: {err}"),
        }
    };

    build("initial", &mut session, &mut cache);

    crate::run_compile_watch_loop(
        vec![std::path::PathBuf::from(path)],
        &format!("skull: watching {path} for changes"),
        |_event| build("rebuild", &mut session, &mut cache),
    )
}

/// `rock run <file> --backend skull` — emit Lua, append a `main()` call if the
/// file defines one, and execute it.
pub fn skull_run(path: &str) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_run_project(path);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let (lua, emits_main) = match skull_emit(path, &src) {
        Ok(l) => l,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let program = if emits_main {
        format!("{lua}\nmain()\n")
    } else {
        lua
    };
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::ablation_report::exit_with_census(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// Render a bone `CompilerError` against a whole project's source set, so spans
/// resolve to the right file. Every discovered source is registered under its
/// id-path (the same id used when parsing into a module).
pub(crate) fn render_project_err(sources: &SourceFiles, e: &CompilerError) -> String {
    render_project_errs(sources, std::slice::from_ref(e))
}

/// [`render_project_err`] over every collected diagnostic, joined into one
/// report — the CLI prints ALL of them, matching the LSP.
pub(crate) fn render_project_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles { files: vec![] };
    for file in &sources.files {
        let f = file.borrow();
        let id = f.file.to_id_path();
        sf.add_or_update_file(source_file_from_path_and_contents(
            id.clone(),
            id,
            f.contents.clone(),
        ));
    }
    crate::bone_backend::render_errs(&sf, errs)
}

/// `rock run <project-dir> --backend skull` — assemble the whole project (all
/// project + dependency `.rock` files + bone stdlib), type-check it through
/// bone, emit Lua via skull, and execute it.
///
/// A game project also needs the stone engine runtime (libstone FFI + the frame
/// loop): the `is_game` branch below stages libstone and execs luajit on
/// `tmp/main.lua`, where stone's global initializer sets up cimgui, reusing the
/// rock backend's run recipe
/// (`build_engine_libs`, `exec_luajit_file`). All games run this way (match3,
/// mag, bitgun, ...). Non-game projects keep the captured-output pipe. See
/// `docs/skull-plan.md` — whole-game milestone (reached).
/// Discover + assemble + bone-typecheck + skull-emit a whole project directory
/// into one Lua program string. The shared core of `run <dir>` and `compile
/// <dir>` (the single-file forms share [`skull_emit_full`]); only the tail
/// differs — `run` wraps `main()` and execs, `compile` writes the string out.
/// Renders against the project sources and exits 1 on any discovery / type /
/// emit failure. The `bool` is the emitter's own `emits_main`, which decides
/// whether the caller appends the `__rock_main_wrapper(main)` entry.
fn skull_emit_project(dir: &str) -> (String, bool) {
    let inputs = match crate::comptime::inputs_for_project_dir(std::path::Path::new(dir)) {
        Ok(inputs) => inputs,
        Err(e) => {
            log::error!(target: "SKULL", "build var resolution failed for project {}: {:#}", dir, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    skull_emit_project_with_inputs(dir, inputs)
}

fn skull_emit_project_with_inputs(
    dir: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> (String, bool) {
    use std::path::PathBuf;
    let renderer_target = match skull_renderer_target(dir) {
        Ok(target) => target,
        Err(e) => {
            log::error!(target: "SKULL", "invalid renderer selection for {}: {}", dir, e);
            std::process::exit(1);
        }
    };
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot load project {}: {}", dir, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    // Game build: drop test-only files (tests/ dirs) so a compile error in a
    // sim/test can't break the running game. See
    // `docs/sim-layering.md`.
    let sources = sources.production_only();

    // Compile core: parse(assemble) + bone check + skull codegen. Excludes
    // discovery (disk read) and the caller's output write, matching the bone
    // backend's `compile` figure so loc/sec lines up across backends.
    let t = Instant::now();
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };

    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            log::error!(target: "SKULL", "bone type-check failed for project {}", dir);
            crate::ablation_report::exit_with_census(1);
        }
    };
    ws.comptime_inputs = inputs;
    let user_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    // Lints are a compiler pass, not an editor accessory: they run on the same
    // typechecked output every build produces, and an `Error` finding refuses
    // the program (`docs/lint-future-plan.md`). The game build dropped `tests/`
    // above, hence ProductionOnly.
    let lint_errs = crate::lint_pass::run_build_lints(
        &ws,
        &sources,
        std::path::Path::new(dir),
        crate::lint_pass::Scope::ProductionOnly,
    );
    if !lint_errs.is_empty() {
        eprintln!("{}", render_project_errs(&sources, &lint_errs));
        log::error!(target: "SKULL", "{} lint error(s) in project {}", lint_errs.len(), dir);
        crate::ablation_report::exit_with_census(1);
    }

    // Project build: modules outside `dir` (the `stone` dependency, stdlib) are
    // libraries and prune to what the project reaches.
    let emit = match skull::mir_emit::emit_workspace_mir_in_for_renderer(
        &ws,
        user_mid,
        Some(dir),
        renderer_target,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", render_project_err(&sources, &e));
            log::error!(target: "SKULL", "skull emit failed for project {}", dir);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let allocation_report_path = allocation_report_path_from_env();
    if let Err(e) =
        write_allocation_report(allocation_report_path.as_deref(), &emit.allocation_report)
    {
        log::error!(target: "SKULL", "cannot write allocation report for {}: {:#}", dir, e);
        crate::ablation_report::exit_with_census(1);
    }
    let emits_main = emit.emits_main;
    let (lua, skipped) = (emit.lua, emit.skipped);
    let compile = t.elapsed();
    crate::bone_backend::log_compile_stats(&sources, &ws, compile, compile, None);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }

    let mut lua = lua;
    if let Err(error) = install_rock_state_metadata(&mut lua, &ws, &spec) {
        eprintln!("{}", render_project_err(&sources, &error));
        log::error!(target: "SKULL", "state metadata planning failed for {}", dir);
        crate::ablation_report::exit_with_census(1);
    }
    // The bone Workspace + HIR modules are dead once `lua` is built -- this is a
    // terminal compile path (`compile`/`run` of a project; never looped in tests
    // or backend-stats), so the only thing after this is the output write / a
    // luajit exec / process exit. Freeing them inline is ~2ms on the critical
    // path (mag: ws ~1ms + modules ~1ms); hand the drop to the rayon pool so it
    // overlaps that tail. `sources` (rock SourceFiles, RefCell-backed and ~0.01ms
    // to drop) stays inline. Both `ws` and `modules` are owned (bone has no
    // Rc/RefCell), hence Send. A `sequential` build drops inline.
    #[cfg(not(feature = "sequential"))]
    rayon::spawn(move || drop((ws, modules)));
    #[cfg(feature = "sequential")]
    drop((ws, modules));
    (lua, emits_main)
}

fn write_allocation_report(
    path: Option<&std::ffi::OsStr>,
    report: &skull::allocation_report::AllocationReport,
) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    if path.is_empty() {
        anyhow::bail!("SKULL_ALLOCATION_REPORT must name an output file");
    }
    let path = std::path::Path::new(path);
    rock_host::project::write_atomic(path, &report.render_text())
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn allocation_report_path_from_env() -> Option<std::ffi::OsString> {
    std::env::var_os("SKULL_ALLOCATION_REPORT").or_else(|| {
        let mut path = std::path::PathBuf::from(std::env::var_os(
            "SKULL_ALLOCATION_ACTIVATION",
        )?);
        path.set_extension("manifest.txt");
        Some(path.into_os_string())
    })
}

/// Attach host-only package grouping and reflection descriptions to the
/// compiler-emitted StateSchema. This is metadata, not a second state-membership
/// oracle: consumers enumerate `_G.__ROCK_STATE_SCHEMA` and read the active
/// Session through its session keys.
fn build_rock_state_metadata(
    ws: &bone_semantic::inference::Workspace,
    spec: &crate::unified_compile::SourceSpec,
) -> std::result::Result<String, CompilerError> {
    use bone_semantic::prelude::ModuleId;
    use std::collections::BTreeMap;

    let package_map = crate::unified_compile::build_package_map(spec);
    let mut metadata: BTreeMap<String, String> = BTreeMap::new();
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        let module_path = m.file.as_str();
        // Longest-prefix match so nested packages resolve to the deepest one.
        let mut matched: Option<&str> = None;
        let mut best_len = 0usize;
        for (pkg_name, pkg_path) in &package_map {
            let pkg_str = pkg_path.to_string_lossy();
            if module_path.starts_with(pkg_str.as_ref()) && pkg_str.len() > best_len {
                best_len = pkg_str.len();
                matched = Some(pkg_name);
            }
        }
        for (_gid, g) in m.globals.iter() {
            let identity = skull_mir::state_schema::global_identity(m.file, g.name);
            let package = matched.map(str::to_string);
            let description = skull::type_desc_lua(ws, g.ty);
            let fields = [
                package.map(|p| format!("package={p:?}")),
                description.map(|d| format!("description={d}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !fields.is_empty() {
                metadata.insert(identity, format!("{{{}}}", fields.join(",")));
            }
        }
    }
    let mut out = String::from("\n_G.__ROCK_STATE_METADATA = {");
    for (identity, fields) in metadata {
        out.push_str(&format!("[{identity:?}]={fields},"));
    }
    out.push_str("}\n");
    out.push_str("for _, entry in ipairs(_G.__ROCK_STATE_SCHEMA or {}) do\n");
    out.push_str("  local metadata = _G.__ROCK_STATE_METADATA[entry.identity]\n");
    out.push_str("  if metadata then\n");
    out.push_str("    entry.package = metadata.package\n");
    out.push_str("    entry.description = metadata.description\n");
    out.push_str("  end\nend\n");
    Ok(out)
}

fn install_rock_state_metadata(
    lua: &mut String,
    ws: &bone_semantic::inference::Workspace,
    spec: &crate::unified_compile::SourceSpec,
) -> std::result::Result<(), CompilerError> {
    let marker = "__rock_state_install_global_bridge()\n";
    let at = lua
        .find(marker)
        .ok_or_else(|| CompilerError::new_no_location(
            "compiler-owned StateSchema is missing its global bridge".to_string(),
        ))?;
    let mut metadata = build_rock_state_metadata(ws, spec)?;
    let scx = skull_mir::lower::ScalarCtx::new(ws);
    metadata.push_str(&build_state_nobox_accessors(ws, &scx)?);
    lua.insert_str(at + marker.len(), &metadata);
    Ok(())
}

/// Build `_G.__ROCK_STATE_NOBOX`, the rebuild/take-apart pair per `nobox`
/// global. A `nobox` global's storage is its leaf globals, so reflection cannot
/// reach it as one Session value; these closures are the physical adapter for
/// the logical StateSchema entry.
///
/// Both directions consume one canonical accessor plan
/// (`nobox_global_accessor_shape`), so they cannot drift from each other or from the
/// storage: `get` is the positional leaf ctor over the plan's paths, and
/// `set` reads the same paths in order. Nested cdata uses the flattened `_lct`
/// representation; ordinary cdata and GC-table values retain `_ct` and `_vt`.
///
fn build_state_nobox_accessors(
    ws: &bone_semantic::inference::Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> std::result::Result<String, CompilerError> {
    use bone_semantic::prelude::ModuleId;

    let mut rows: Vec<String> = Vec::new();
    let state_slots = skull::mir_emit::lua_global_state_slots(ws, scx)?;
    for m in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_gid, g) in m.globals.iter().filter(|(_, global)| !global.is_const) {
            let Some(acc) = skull_mir::lower::nobox_global_accessor_shape(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?
            else { continue };
            let skull_mir::lower::NoboxGlobalAccessorShape {
                ty_name, paths, nullable, storage, ctor_suffix: suffix,
                presence_path: _, unbuildable,
            } = acc;
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            // The leaves are emitted symbols, matching the Session slot map.
            let all_leaves = plan
                .leaf_names(skull_mir::mangle::symbol_of_key(g.name))
                .ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "nobox global `{}` has an accessor shape but no leaf-name plan",
                        g.name
                    ))
                })?;
            // A nullable global leads with a presence leaf that has no path: it is
            // not a field, so it is branched on rather than read. Everything below
            // pairs the REMAINING leaves with the paths.
            let (present, leaves) = if nullable {
                let Some((p, rest)) = all_leaves.split_first() else {
                    return Err(CompilerError::new_no_location(format!(
                        "nullable nobox global `{}` has no presence leaf",
                        g.name
                    )));
                };
                (Some(*p), rest.to_vec())
            } else {
                (None, all_leaves)
            };
            if leaves.len() != paths.len() {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` has {} leaves but {} leaf paths",
                    g.name,
                    leaves.len(),
                    paths.len()
                )));
            }
            // Both accessor directions are POSITIONAL over the leaf sequence, so
            // they are sound only where the ctor is too. `nobox_global_accessor_shape`
            // makes that call; guessing is how a leak becomes a miscompile, so an
            // unbuildable shape rejects rather than emit a wrong accessor.
            if let Some(why) = &unbuildable {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` of type `{ty_name}` cannot be rebuilt: {why}",
                    g.name
                )));
            }
            let all_leaf_names = if let Some(present) = present {
                std::iter::once(present).chain(leaves.iter().copied()).collect::<Vec<_>>()
            } else {
                leaves.clone()
            };
            let state_keys = all_leaf_names
                .iter()
                .map(|leaf| {
                    state_slots.get(leaf).copied().ok_or_else(|| {
                        CompilerError::new_no_location(format!(
                            "nobox global `{}` leaf `{leaf}` has no Lua Session slot",
                            g.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (present_key, value_keys) = if nullable {
                let (present, values) = state_keys
                    .split_first()
                    .expect("a nullable shape has a presence Session slot");
                (Some(*present), values.to_vec())
            } else {
                (None, state_keys)
            };
            let args = value_keys
                .iter()
                .map(|key| format!("__rock_state_get({key})"))
                .collect::<Vec<_>>()
                .join(", ");
            let writes = leaves
                .iter()
                .zip(&paths)
                // Flattened leaf tables store dotted paths as literal keys;
                // declared cdata fields remain ordinary nested field reads.
                .zip(&value_keys)
                .map(|((_, p), key)| {
                    let value = if storage == skull_mir::lower::ValueStorage::LeafTable {
                        format!("v[{:?}]", p.as_str())
                    } else {
                        format!("v.{}", p.as_str())
                    };
                    format!("__rock_state_set({key}, {value})")
                })
                .collect::<Vec<_>>()
                .join("; ");
            let (get, set) = match present {
                // Presence gates BOTH directions: rebuilding an absent value would
                // hand reflection a struct of nils, and a write that left the leaves
                // behind would let a later read resurrect the old value.
                Some(_) => (
                    format!(
                        "if not __rock_state_get({}) then return nil end return {ty_name}{suffix}({args})",
                        present_key.as_ref().expect("nullable accessor has a presence key")
                    ),
                    format!(
                        "__rock_state_set({}, v ~= nil); if v ~= nil then {writes} else {} end",
                        present_key.as_ref().expect("nullable accessor has a presence key"),
                        leaves
                            .iter()
                            .zip(&value_keys)
                            .map(|(_, key)| format!("__rock_state_clear({key})"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ),
                None => (format!("return {ty_name}{suffix}({args})"), writes),
            };
            rows.push(format!(
                "[{:?}]={{get=function() {get} end,set=function(v) {set} end}},",
                skull_mir::state_schema::global_identity(m.file, g.name)
            ));
        }
    }
    // Assigned even when EMPTY. A full reload re-executes the chunk in the live
    // process, so what the new chunk omits it inherits: emitting nothing here
    // would leave the previous build's accessors installed, and changing the last
    // `nobox` global to an ordinary one would keep reflection reading leaf globals
    // that no longer back it.
    Ok(format!("\n_G.__ROCK_STATE_NOBOX = {{{}}}\n", rows.concat()))
}

/// Collect `@sim` info from the bone workspace -- the skull analog of rock's
/// `sim_registry::collect_sims`, over bone HIR. Returns each sim's wire name
/// (the `@sim("name")` arg, else the fn name) + its pre-spawn `@sim_env` pairs,
/// with `@sim_render` sugar (`SIM_SKIP_RENDER=0`) applied first so an explicit
/// `@sim_env` can override. Workspace/source order; the runner applies each
/// pair via `Command::env` (later wins). Same shape that drives the emitted
/// `register_sim` registry, so the wire set the runner sees matches what the
/// game can dispatch.
fn collect_skull_sims(
    ws: &bone_semantic::inference::Workspace,
) -> Vec<(String, Vec<(String, String)>)> {
    use bone_semantic::hir::AnnoValue;

    // The `@sim` walk + wire derivation is shared with skull's emitted registry
    // (`skull::mir_emit::collect_sim_fns`) so the runner's wire names match what
    // the game registered. Here we additionally read each sim's `@sim_env`
    // pairs (a runner-only concern): `@sim_render` sugar first so an explicit
    // `@sim_env` can override.
    skull::mir_emit::collect_sim_fns(ws)
        .into_iter()
        .map(|(f, wire)| {
            let mut env_pairs = Vec::new();
            if f.annotations.iter().any(|a| a.is_builtin("sim_render")) {
                env_pairs.push(("SIM_SKIP_RENDER".to_string(), "0".to_string()));
            }
            for ann in &f.annotations {
                if !ann.is_builtin("sim_env") {
                    continue;
                }
                let (Some(AnnoValue::String(k)), Some(AnnoValue::String(v))) = (
                    ann.args.first().map(|a| &a.value),
                    ann.args.get(1).map(|a| &a.value),
                ) else {
                    continue;
                };
                env_pairs.push((k.to_string(), v.to_string()));
            }
            (wire, env_pairs)
        })
        .collect()
}

/// Build the skull game program for `dir`, write the game-shaped `main.lua`
/// (preamble + entry + the `@sim` registry) to `main_path`, and collect the
/// project's `@sim` wire names + env pairs from the SAME workspace build. The
/// skull counterpart of rock's `compile_sims` -- one type-check feeds both the
/// emitted registry and the runner's wire/env list, so they cannot drift.
pub fn skull_compile_game_and_collect_sims(
    dir: &str,
    main_path: &std::path::Path,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, SkullStone)> {
    skull_compile_game_and_collect_sims_with_allocation(dir, main_path, None, None)
}

/// `callee_binding` overrides the emitted callee shape for THIS session only;
/// `None` reads the process shape. The sim runner's hot-reload lane compiles a
/// second session with `Some(CalleeBindingMode::Global)` so its `reload_edit`
/// injection still produces an `incremental.lua` overlay.
pub fn skull_compile_game_and_collect_sims_with_allocation(
    dir: &str,
    main_path: &std::path::Path,
    allocation_manifest: Option<&std::path::Path>,
    callee_binding: Option<skull::CalleeBindingMode>,
) -> Result<(Vec<String>, HashMap<String, Vec<(String, String)>>, SkullStone)> {
    // Session-based build (same machinery as `rock stone`, tests included so the
    // `@sim` bodies are in the program). The live SkullStone is returned so the
    // runner can serve mid-sim `reload_edit` ops through the REAL incremental
    // path (`docs/sim-reload-harness-plan.md`).
    let incremental_path = main_path.with_file_name("incremental.lua");
    let stone = match allocation_manifest {
        Some(path) => SkullStone::init_with_tests_and_allocation_report(
            dir,
            main_path,
            &incremental_path,
            path,
        ),
        None => SkullStone::init_with_tests_and_callee_binding(
            dir,
            main_path,
            &incremental_path,
            callee_binding,
        ),
    }
    .map_err(|e| anyhow::anyhow!("cannot compile project {dir}: {e}"))?;

    let sims = collect_skull_sims(stone.project.workspace());
    let mut wire_names = Vec::with_capacity(sims.len());
    let mut env_by_wire = HashMap::with_capacity(sims.len());
    for (wire, env) in sims {
        wire_names.push(wire.clone());
        env_by_wire.insert(wire, env);
    }
    Ok((wire_names, env_by_wire, stone))
}

/// `rock compile <project-dir> --backend skull` — assemble + type-check + emit
/// the whole project to Lua and write it to `output` (or stdout). The directory
/// analogue of the single-file [`skull_compile_single`] path; shares
/// [`skull_emit_project`] with `run`.
fn skull_compile_project(dir: &str, output: Option<String>) -> Result<()> {
    let inputs = crate::comptime::inputs_for_project_dir(std::path::Path::new(dir))?;
    skull_compile_project_with_inputs(dir, output, inputs)
}

pub fn skull_compile_project_with_inputs(
    dir: &str,
    output: Option<String>,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<()> {
    use std::path::PathBuf;
    let (emit, emits_main) = skull_emit_project_with_inputs(dir, inputs);
    // A game's runnable main.lua is the same shape run/stone produce via
    // write_full_main: preamble (game source dir) + emit +
    // __rock_main_wrapper(main). Without the preamble game asset paths are
    // wrong; without the wrapper it never starts.
    let lua = if rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false) {
        skull_game_main(&skull_game_preamble(dir)?, &emit, emits_main)
    } else if emits_main {
        format!("{emit}\n__rock_main_wrapper(main)\n")
    } else {
        emit
    };
    match output {
        Some(out) => {
            rock_host::project::write_atomic(&out, &lua)?;
            log::info!(target: "SKULL", "{} -> {}", dir, out);
        }
        None => print!("{}", lua),
    }
    Ok(())
}

fn skull_run_project(dir: &str) -> Result<()> {
    use std::path::PathBuf;
    ensure_renderer_feature(dir)?;
    let is_game = rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false);
    if is_game {
        crate::package::prepare_sound_bank(dir)?;
    }
    let (lua, emits_main) = skull_emit_project(dir);

    // Game projects need the rock backend's run recipe, not a bare stdin
    // pipe: stage libstone into tmp/ (build_engine_libs), run luajit on a
    // FILE with LD_LIBRARY_PATH set (build_luajit_command) so
    // `ffi.load("libstone.so")` resolves, and inherit stdio so the
    // window/input work. Non-game projects keep the captured-output pipe.
    if is_game {
        if !std::env::var("ROCK_SKIP_ENGINE_BUILD").is_ok_and(|v| v == "1") {
            if let Err(e) = rock_host::engine_build::build_engine_libs(dir) {
                log::error!(target: "SKULL", "engine build failed: {}", e);
                crate::ablation_report::exit_with_census(1);
            }
        }
        // Game preamble + `__rock_main_wrapper(main)` entry, built by the same
        // helpers the stone two-file watch path uses (so both write an
        // identical tmp/main.lua), and wipe any stale overlay so the engine's
        // hot-reload watcher can't load a leftover incremental.lua over the
        // fresh baseline.
        let lua_file = rock_host::project::default_main_lua_path();
        if let Err(e) = write_full_main(
            &skull_game_preamble(dir)?,
            &lua,
            emits_main,
            &lua_file,
            &lua_file.with_file_name("incremental.lua"),
        ) {
            log::error!(target: "SKULL", "cannot write {}: {}", lua_file.display(), e);
            crate::ablation_report::exit_with_census(1);
        }
        // No timeout: games run an infinite frame loop.
        return crate::compile_cmd::exec_luajit_file(&lua_file, None, &[]);
    }

    // Non-game: append the entry wrapper and pipe the captured output.
    let program = if emits_main {
        format!("{lua}\n__rock_main_wrapper(main)\n")
    } else {
        lua
    };
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::ablation_report::exit_with_census(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// `rock test <paths> --backend skull` — for every `.rock` file, run the skull
/// pipeline and the luajit validity gate; report pass/fail + a bucketed
/// failure-reason histogram (the skull analogue of the bone test histogram).
/// Split `paths` into test UNITS: isolated files (root `tests/`: independent
/// programs that would collide in one workspace) and whole `stick.toml`
/// packages built as ONE workspace each. A cross-file lib can only be tested
/// the second way -- per-file, any file referencing a sibling's symbol fails to
/// compile. Shared by every test executor (luajit + JS).
pub(crate) fn collect_test_units(paths: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut files = Vec::new();
    let mut package_dirs = Vec::new();
    for path in paths {
        if is_test_package_dir(path) {
            package_dirs.push(path.clone());
        } else if let Some(pkg) = is_package_tests_dir(path) {
            package_dirs.push(pkg);
        } else {
            files.extend(collect_rock_files(path)?);
        }
    }
    files.sort();
    files.dedup();
    package_dirs.sort();
    package_dirs.dedup();
    Ok((files, package_dirs))
}

pub fn skull_test(paths: &[String]) -> Result<()> {
    crate::lint_pass::set_corpus_mode();
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();

    let (files, package_dirs) = collect_test_units(paths)?;

    let start = Instant::now();
    let mut emit_ok = 0usize; // units: emit valid Lua (passes or test-fails)
    let mut exec_pass = 0usize; // units: emit + all @tests pass
    let mut failed = 0usize; // units: emit/load failure
    let mut tests_total = 0usize; // individual @test functions executed
    let mut tests_passed = 0usize; // individual @test functions that passed
    let mut reason_counts: HashMap<String, usize> = HashMap::new();
    let mut details: Vec<(String, String)> = Vec::new();

    // bone can panic during inference on some inputs (a bone bug, not ours).
    // Silence the default panic print during the sweep and catch each unit's
    // panic so one bad unit doesn't abort the whole measurement — it's reported
    // as a `bone panic` failure instead.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    // Run every unit (files first, then packages), collecting labelled outcomes
    // so both kinds fold through one reporting path.
    type UnitResult = std::thread::Result<std::result::Result<SkullOutcome, SkullFailure>>;
    let mut outcomes: Vec<(String, UnitResult)> = Vec::new();
    for file in &files {
        let outcome: UnitResult = match fs_err::read_to_string(file) {
            Ok(src) => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| skull_build(file, &src)))
            }
            Err(e) => Ok(Err(SkullFailure { reason: e.to_string(), rendered: e.to_string() })),
        };
        outcomes.push((short_path(file).to_string(), outcome));
    }
    for dir in &package_dirs {
        let outcome: UnitResult =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| skull_build_package(dir)));
        outcomes.push((short_path(dir).to_string(), outcome));
    }

    for (label, outcome) in outcomes {
        match outcome {
            Ok(Ok(SkullOutcome::Pass { tests })) => {
                emit_ok += 1;
                exec_pass += 1;
                tests_total += tests;
                tests_passed += tests;
                if verbose {
                    println!("{:.<60} {}", label, "PASS".green());
                }
            }
            Ok(Ok(SkullOutcome::TestFail { passed, fails })) => {
                emit_ok += 1;
                tests_total += passed + fails.len();
                tests_passed += passed;
                println!(
                    "{:.<60} {}  {}",
                    label,
                    "TESTFAIL".yellow(),
                    format!("{} @test(s) failed", fails.len()).dimmed()
                );
                *reason_counts
                    .entry("@test execution failed".to_string())
                    .or_insert(0) += 1;
                if verbose {
                    details.push((label, fails.join("\n")));
                }
            }
            Ok(Err(SkullFailure { reason, rendered })) => {
                failed += 1;
                println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, rendered));
                }
            }
            Err(panic) => {
                failed += 1;
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let reason = "bone panic (inference)".to_string();
                println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, msg));
                }
            }
        }
    }

    std::panic::set_hook(prev_hook);

    if !reason_counts.is_empty() {
        let mut rows: Vec<(String, usize)> = reason_counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        println!("\n{}", "=== failure reasons (skull) ===".bold());
        let shown = rows.len().min(25);
        for (reason, count) in rows.iter().take(shown) {
            println!("  {:>4}  {}", count.to_string().yellow(), reason);
        }
        if rows.len() > shown {
            println!("  {:>4}  ({} more distinct reasons)", "...", rows.len() - shown);
        }
    }

    if verbose && !details.is_empty() {
        println!("\n{}", "=== failure details ===".bold());
        for (file, rendered) in &details {
            println!("\n--- {} ---\n{}", file, rendered);
        }
    } else if failed > 0 || emit_ok > exec_pass {
        println!(
            "\n{}",
            "(re-run with ROCK_TEST_VERBOSE=1 for full per-file errors)".dimmed()
        );
    }

    let total = emit_ok + failed;
    let test_fails = emit_ok - exec_pass;
    let tests_failed = tests_total - tests_passed;
    println!(
        "\nskull backend: {}/{} @tests passed ({} failed) across {} files ({:.2}s)",
        tests_passed.to_string().green(),
        tests_total,
        if tests_failed == 0 {
            tests_failed.to_string().green()
        } else {
            tests_failed.to_string().red()
        },
        total,
        start.elapsed().as_secs_f64(),
    );
    println!(
        "  files: {} all-pass, {} emit valid Lua ({} with test failures), {} emit-fail, {} total",
        exec_pass.to_string().green(),
        emit_ok.to_string().green(),
        test_fails.to_string().yellow(),
        if failed == 0 {
            failed.to_string().green()
        } else {
            failed.to_string().red()
        },
        total,
    );

    // LSP fixtures run through the bone-backed LSP (skull's language server),
    // via the same dispatch helper rock's `test` uses. Failures count toward
    // the non-zero exit alongside @test failures.
    let mut lsp_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) = crate::lsp_test::run_lsp_suite(
            std::path::Path::new(path),
            crate::Backend::Skull,
            verbose,
        )?;
        lsp_failed += f;
    }

    // Error-test fixtures (`.rock.err`) verify rejection, which happens in the
    // shared bone front end before codegen -- so skull folds them in too.
    let mut err_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) = crate::error_test::run_error_suite(
            std::path::Path::new(path),
            crate::Backend::Skull,
            verbose,
        )?;
        err_failed += f;
    }

    // The Stage-0e census, before the exit below: this path leaves the process
    // with `process::exit`, which runs no destructor and never returns to
    // `main`, so a report installed only there would silently never print.
    let ablation_fatals = crate::ablation_report::report("rock test");

    if tests_failed > 0
        || test_fails > 0
        || failed > 0
        || lsp_failed > 0
        || err_failed > 0
        || ablation_fatals
    {
        // Plain exit: the census above already drained the registry, so routing
        // this through `exit_with_census` would print an empty second one.
        std::process::exit(1);
    }
    Ok(())
}

// luajit run + stdout-normalization helpers, shared by the hot-reload gates below.

/// Per-test execution outcome (clean stdout vs errored), as the gates read it.
#[cfg(test)]
#[path = "skull_backend_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// skull-incremental-profile
// ---------------------------------------------------------------------------

struct SkullIncrRow {
    name: String,
    update_ms: f64,
    emit_ms: f64,
    reinferred: usize,
    matches: bool,
}

pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Force a body re-cook by injecting a space at the start of the first top-level
/// `fn`'s first body line. Whitespace INSIDE the body's block span changes the
/// body hash (so the Session re-cooks + re-infers that body) but not the lowered
/// HIR, so skull re-emits a byte-identical chunk -- the touch exercises the
/// incremental path while the output stays comparable to the fresh baseline.
///
/// The space must land inside the block span `[bs..be]`, NOT in the gap right
/// after `{` (which classifies as LayoutOnly -> 0 bodies re-inferred), so anchor
/// it after the first newline following the brace. `None` for files with no
/// touchable top-level `fn` body.
pub(crate) fn body_touch(text: &str) -> Option<String> {
    let fn_pos = if text.starts_with("fn ") {
        0
    } else {
        text.find("\nfn ")? + 1
    };
    let brace = text[fn_pos..].find('{')? + fn_pos;
    let nl = text[brace..].find('\n')? + brace; // first newline after the brace
    let mut out = String::with_capacity(text.len() + 1);
    out.push_str(&text[..=nl]); // through the newline
    out.push(' '); // start of the first body line -> inside the block span
    out.push_str(&text[nl + 1..]);
    Some(out)
}

/// A REAL semantic body edit: flip the first single ASCII digit inside the body
/// of `fn <name>` (d -> d-1, '0'->'1'). Unlike `body_touch`'s whitespace, this
/// is a genuine 1-char constant change like editing mag `update_mobs`
/// `tick_anim(mob.anim, 3, ..)` 3->2 (alternated back to 3 by re-applying the
/// ORIGINAL text). Returns None if the fn or a body digit isn't found.
pub(crate) fn body_digit_flip(text: &str, name: &str) -> Option<String> {
    let fn_pos = text.find(&format!("fn {name}"))?;
    let brace = text[fn_pos..].find('{')? + fn_pos;

    // Bound the body by brace-matching so we only touch THIS function's digits.
    let mut depth = 0i32;
    let mut end = text.len();
    for (i, c) in text[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace + i;
                    break;
                }
            }
            _ => {}
        }
    }

    let rel = text[brace..end].find(|c: char| c.is_ascii_digit())?;
    let abs = brace + rel;
    let d = text.as_bytes()[abs];
    let flipped = if d == b'0' { b'1' } else { d - 1 };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..abs]);
    out.push(flipped as char);
    out.push_str(&text[abs + 1..]);
    Some(out)
}

/// `rock skull-incremental-profile <project>` -- profile skull's INCREMENTAL
/// recompile path (bone `Session` per-file cook + `emit_incremental`, what `rock
/// watch --backend skull` runs). Per project file: force a body re-cook, time
/// `update_file` + `emit_incremental`, verify the incremental output is
/// byte-identical to the fresh `emit_full` baseline, then revert so the next
/// file measures from the same baseline. The skull analogue of
/// `incremental-profile` (which is rock-CompilationStorage-only). Exits 1 on any
/// byte divergence (an incremental-emit soundness bug).
pub fn skull_incremental_profile(path: &str) -> Result<()> {
    use std::path::PathBuf;

    let renderer_target = skull_renderer_target(path)?;
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(path));
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let files: Vec<(String, String)> = sources
        .files
        .iter()
        .map(|f| {
            let f = f.borrow();
            (f.file.to_id_path(), f.contents.clone())
        })
        .collect();

    eprintln!("=== Skull Incremental Profile: {path} ===\n");

    let t_warm = Instant::now();
    let mut session =
        crate::bone_backend::bone_session_for_emission(files.clone(), std::path::Path::new(path))
            .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;
    let warm_ms = ms(t_warm);

    let (baseline_lua, mut cache, full_ms) = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let t_full = Instant::now();
        let (emit, c) = skull::mir_emit::emit_full_in_for_renderer(
            ws,
            mid,
            Some(path),
            renderer_target,
        )
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
        (emit.lua, c, ms(t_full))
    };

    eprintln!(
        "warm session {warm_ms:.1}ms | fresh emit_full {full_ms:.1}ms ({} lines of Lua)\n",
        baseline_lua.lines().count()
    );

    // Profile only the project's own files (skip the stone engine + other deps
    // discovery pulls in): match the project directory's basename as a path
    // component, e.g. `mag` -> only `.../mag/*.rock`.
    let proj_dir = format!(
        "{}/",
        std::path::Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    let mut rows: Vec<SkullIncrRow> = Vec::new();
    let mut skipped = 0usize;

    for (fpath, text) in &files {
        if !fpath.contains(&proj_dir) {
            continue;
        }
        let Some(touched) = body_touch(text) else {
            skipped += 1;
            continue;
        };

        let t_u = Instant::now();
        let upd = session
            .update_file(fpath, &touched)
            .map_err(|e| anyhow::anyhow!("update_file({fpath}): {}", e.message))?;
        let update_ms = ms(t_u);

        let (emit_ms, incr_lua) = {
            let ws = session.workspace();
            let t_e = Instant::now();
            let incr = skull::mir_emit::emit_incremental(ws, &mut cache, &upd.reinferred)
                .map_err(|e| anyhow::anyhow!("emit_incremental({fpath}): {}", e.message))?;
            (ms(t_e), incr.lua)
        };

        rows.push(SkullIncrRow {
            name: std::path::Path::new(fpath)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| fpath.clone()),
            update_ms,
            emit_ms,
            reinferred: upd.reinferred.len(),
            matches: incr_lua == baseline_lua,
        });

        // Revert so the next file starts from the baseline session + cache.
        let rev = session
            .update_file(fpath, text)
            .map_err(|e| anyhow::anyhow!("revert({fpath}): {}", e.message))?;
        let ws = session.workspace();
        skull::mir_emit::emit_incremental(ws, &mut cache, &rev.reinferred)
            .map_err(|e| anyhow::anyhow!("revert emit({fpath}): {}", e.message))?;
    }

    // --- table ---
    let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(8).max(8);
    eprintln!(
        "{:<w$}  {:>9}  {:>8}  {:>10}  {:>8}  {:>6}",
        "Module", "Update", "Emit", "Reinferred", "vs Full", "Status",
        w = max_name
    );
    eprintln!("{}", "-".repeat(max_name + 50));
    let mut sorted: Vec<&SkullIncrRow> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (b.update_ms + b.emit_ms)
            .partial_cmp(&(a.update_ms + a.emit_ms))
            .unwrap()
    });
    for r in &sorted {
        let total = r.update_ms + r.emit_ms;
        let ratio = if full_ms > 0.0 {
            format!("{:.0}%", total / full_ms * 100.0)
        } else {
            "-".into()
        };
        eprintln!(
            "{:<w$}  {:>7.2}ms  {:>6.2}ms  {:>10}  {:>8}  {:>6}",
            r.name,
            r.update_ms,
            r.emit_ms,
            r.reinferred,
            ratio,
            if r.matches { "OK" } else { "DIFF!" },
            w = max_name
        );
    }

    // --- summary ---
    let n = rows.len().max(1) as f64;
    let avg_update: f64 = rows.iter().map(|r| r.update_ms).sum::<f64>() / n;
    let avg_emit: f64 = rows.iter().map(|r| r.emit_ms).sum::<f64>() / n;
    let diffs: Vec<&SkullIncrRow> = rows.iter().filter(|r| !r.matches).collect();
    eprintln!("\nSummary:");
    eprintln!("   Fresh emit_full:        {full_ms:.1}ms");
    eprintln!(
        "   Avg incremental:        {:.2}ms ({:.0}% of full)  [update {:.2}ms + emit {:.2}ms]",
        avg_update + avg_emit,
        (avg_update + avg_emit) / full_ms * 100.0,
        avg_update,
        avg_emit
    );
    eprintln!("   Modules profiled:       {}  ({} skipped: no touchable fn body)", rows.len(), skipped);

    if diffs.is_empty() {
        eprintln!(
            "\nAll {} modules: incremental emit byte-identical to fresh. Incremental is correct.",
            rows.len()
        );
        Ok(())
    } else {
        eprintln!("\n{} INCREMENTAL EMIT DIVERGENCES (incremental != fresh):", diffs.len());
        for r in &diffs {
            eprintln!("   {}", r.name);
        }
        crate::ablation_report::exit_with_census(1);
    }
}

/// `rock iter-incremental <project>` -- tight-loop sampling profiler for the REAL
/// stone incremental path. Warm a Session + `emit_full` once, then repeatedly
/// `update_file` + `emit_overlay` ONE body, alternating between two whitespace
/// states so every pass is a genuine change (not a content-hash no-op). Reports
/// the timing distribution (mean/p50/p99/min/max/stddev) + overlay size + the
/// per-phase breakdown of BOTH halves: `update_file` (clone/cook/lower/infer, from
/// bone's `phase_timings`) and `emit_overlay`, plus verdict lints. It calls the EXACT same
/// `session.update_file` + `skull::mir_emit::emit_overlay` that
/// `SkullStone::recompile` runs on a body edit (no reassembly -- the two-file
/// split's whole point), so the numbers are the live recompile path, just sampled
/// in a tight loop instead of off disk.
///
/// Post-rung-2, `update_file` re-lowers + re-infers ONLY the changed body (not the
/// whole module). `SKULL_ITER_FILE=<substr>` targets a file (whitespace touch of
/// its first fn); `SKULL_ITER_FN=<name>` profiles a REAL constant edit -- flips
/// the first digit in that fn's body, alternating with the original (e.g.
/// `SKULL_ITER_FN=update_mobs` flips mag's `tick_anim(.., 3, ..)` 3<->2).
/// `SKULL_ITER_WRITE=1` additionally measures the production atomic overlay
/// write into a temporary output path.
fn iter_incremental_sources(path: &str) -> Result<rock_core::SourceFiles> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(path));
    Ok(crate::unified_compile::discover_sources(&spec)?.production_only())
}

pub fn skull_iter_incremental(path: &str, iters: usize, warmup: usize) -> Result<()> {
    let renderer_target = skull_renderer_target(path)?;
    let sources = iter_incremental_sources(path)?;
    let files: Vec<(String, String)> = sources
        .files
        .iter()
        .map(|f| {
            let f = f.borrow();
            (f.file.to_id_path(), f.contents.clone())
        })
        .collect();

    let mut session =
        crate::bone_backend::bone_session_for_emission(files.clone(), std::path::Path::new(path))
            .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;

    let mut cache = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let (_emit, c) = skull::mir_emit::emit_full_in_for_renderer(
            ws,
            mid,
            Some(path),
            renderer_target,
        )
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
        c
    };

    // Pick the first project file with a touchable top-level fn body, and build
    // two whitespace states (1 vs 2 spaces inside that body) so consecutive
    // updates always differ.
    let proj_dir = format!(
        "{}/",
        std::path::Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );
    // SKULL_ITER_FN=<name>: profile a REAL constant edit (digit flip) in that
    // fn's body, alternating modified <-> original -- e.g.
    // `SKULL_ITER_FN=update_mobs rock iter-incremental mag` flips `tick_anim(..,
    // 3, ..)` 3<->2. Else: auto-pick a file and inject whitespace (body_touch).
    let (target, mut state_a, mut state_b) = if let Ok(fname) = std::env::var("SKULL_ITER_FN") {
        let (p, t) = files
            .iter()
            .find(|(p, t)| p.contains(&proj_dir) && t.contains(&format!("fn {fname}")))
            .ok_or_else(|| anyhow::anyhow!("no project file with `fn {fname}` in {path}"))?;
        let flipped = body_digit_flip(t, &fname)
            .ok_or_else(|| anyhow::anyhow!("no body digit to flip in `fn {fname}`"))?;
        // alternate flipped <-> original (a genuine 1-char body edit each pass)
        (p.clone(), flipped, t.clone())
    } else {
        // Pick the first project file whose touch actually re-cooks a body (>=1
        // re-inferred): some files' first fn lands the injected whitespace outside
        // the block span (LayoutOnly -> 0 bodies). Trial each via the live Session,
        // reverting after so the baseline is restored (no emit, so the cache is
        // untouched). Profiling a 0-reinferred file would only measure reassembly.
        let want_file = std::env::var("SKULL_ITER_FILE").ok();
        let mut chosen: Option<(String, String)> = None;
        for (p, t) in &files {
            if !p.contains(&proj_dir) {
                continue;
            }
            if let Some(w) = &want_file {
                if !p.contains(w.as_str()) {
                    continue;
                }
            }
            let Some(touched) = body_touch(t) else { continue };
            let upd = session
                .update_file(p, &touched)
                .map_err(|e| anyhow::anyhow!("trial update_file({p}): {}", e.message))?;
            let reinferred = upd.reinferred.len();
            session
                .update_file(p, t)
                .map_err(|e| anyhow::anyhow!("trial revert({p}): {}", e.message))?;
            if reinferred >= 1 {
                chosen = Some((p.clone(), t.clone()));
                break;
            }
        }
        let (target, orig) = chosen
            .ok_or_else(|| anyhow::anyhow!("no project file whose touch re-cooks a body in {path}"))?;
        let a = body_touch(&orig).unwrap();
        let b = body_touch(&a).unwrap();
        (target, a, b)
    };
    if std::env::var_os("SKULL_ITER_REVERSE").is_some() {
        std::mem::swap(&mut state_a, &mut state_b);
    }

    let mut lint_cache = bone_lints::IncrementalLintCache::default();
    let initial_lint_errors = crate::lint_pass::run_build_lints_cached(
        &mut lint_cache,
        session.workspace(),
        &sources,
        std::path::Path::new(path),
        crate::lint_pass::Scope::WholeProject,
        &bone_lints::LintCacheUpdate::full(),
    );
    if !initial_lint_errors.is_empty() {
        return Err(anyhow::anyhow!(
            "iter-incremental fixture produced {} initial lint error(s)",
            initial_lint_errors.len()
        ));
    }

    let iters = iters.max(1);
    let target_name = std::path::Path::new(&target)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    let write_disk = std::env::var_os("SKULL_ITER_WRITE").is_some();
    let write_dir = tempfile::tempdir().map_err(|e| anyhow::anyhow!("create iter output: {e}"))?;
    let write_path = write_dir.path().join("incremental.lua");
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let mut disk_samples: Vec<f64> = Vec::with_capacity(iters);
    let mut last_update = 0.0;
    let mut last_diag = 0.0;
    let mut last_lint = 0.0;
    let mut last_emit = 0.0;
    let mut last_write = 0.0;
    let mut last_bytes = 0usize;
    let mut last_reinferred = 0usize;
    let mut last_body_meta = (0usize, false, 0usize, 0isize);
    let mut last_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let mut last_update_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();

    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &state_a } else { &state_b };

        let t_u = Instant::now();
        let upd = session
            .update_file(&target, text)
            .map_err(|e| anyhow::anyhow!("update_file: {}", e.message))?;
        if let Some(source) = sources
            .files
            .iter()
            .find(|source| source.borrow().file.to_id_path() == target)
        {
            source.borrow_mut().contents = text.clone();
        }
        let update_ms = ms(t_u);
        // bone's per-edit phase breakdown (clone/cook/lower/infer), set by
        // rebuild_body_incremental inside the SAME update_file the stone loop calls.
        let update_phases = session.workspace().phase_timings.clone();
        let span_edits = upd
            .body_update
            .edit
            .as_ref()
            .map(|edit| {
                vec![skull::instance_graph::SpanEdit {
                    file: edit.file,
                    at: edit.at,
                    byte_delta: edit.byte_delta,
                    line_delta: edit.line_delta,
                }]
            })
            .unwrap_or_default();

        // `SkullStone::recompile` calls `diagnostics()` inside its `cook` timing --
        // an O(workspace) error scan + sort over every module. Measure it here too
        // so the iter-incremental "cook" matches the live one (the live cook ALSO
        // includes disk read of the changed file, which an in-memory loop can't).
        let t_d = Instant::now();
        let _ = session.diagnostics();
        let diag_ms = ms(t_d);

        let t_l = Instant::now();
        let lint_change = if matches!(upd.tier, bone_incremental::session::UpdateTier::BodyOnly) {
            bone_lints::LintCacheUpdate::body_only(
                [ustr::Ustr::from(target.as_str())],
                upd.reinferred.iter().copied(),
                upd.body_update.edit.iter().map(|edit| bone_lints::TextEditDelta {
                    file: edit.file,
                    at: edit.at,
                    byte_delta: edit.byte_delta,
                    line_delta: edit.line_delta,
                }),
            )
        } else {
            bone_lints::LintCacheUpdate::full()
        };
        let lint_errs = crate::lint_pass::run_build_lints_cached(
            &mut lint_cache,
            session.workspace(),
            &sources,
            std::path::Path::new(path),
            crate::lint_pass::Scope::WholeProject,
            &lint_change,
        );
        if !lint_errs.is_empty() {
            return Err(anyhow::anyhow!(
                "iter-incremental fixture produced {} lint error(s)",
                lint_errs.len()
            ));
        }
        let lint_ms = ms(t_l);

        // Profile the REAL stone path: `emit_overlay` (no reassembly), matching
        // what `SkullStone::recompile` writes on a body edit.
        let (emit_ms, overlay, phases) = {
            let ws = session.workspace();
            let t_e = Instant::now();
            let e = skull::mir_emit::emit_overlay_with_span_edits(
                ws,
                &mut cache,
                &upd.reinferred,
                &span_edits,
            )
                .map_err(|e| anyhow::anyhow!("emit_overlay: {}", e.message))?;
            (ms(t_e), e.lua, e.phases)
        };
        let overlay_bytes = overlay.len();
        let write_ms = if write_disk {
            let t_w = Instant::now();
            rock_host::project::write_atomic(&write_path, &overlay)
                .map_err(|e| anyhow::anyhow!("iter-incremental write: {e}"))?;
            ms(t_w)
        } else {
            0.0
        };

        if i >= warmup {
            let backend_ms = update_ms + diag_ms + lint_ms + emit_ms;
            samples.push(backend_ms);
            if write_disk {
                disk_samples.push(backend_ms + write_ms);
            }
            last_update = update_ms;
            last_diag = diag_ms;
            last_lint = lint_ms;
            last_emit = emit_ms;
            last_bytes = overlay_bytes;
            last_write = write_ms;
            last_reinferred = upd.reinferred.len();
            last_body_meta = (
                upd.body_update.semantic_bodies.len(),
                upd.body_update.span_only,
                upd.body_update.edit.as_ref().map_or(0, |edit| edit.at),
                upd.body_update.edit.as_ref().map_or(0, |edit| edit.byte_delta),
            );
            last_phases = phases;
            last_update_phases = update_phases;
        }
    }

    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let var = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let stddev = var.sqrt();
    let cv = if mean > 0.0 { stddev / mean * 100.0 } else { 0.0 };
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];

    eprintln!("=== Skull Iter-Incremental: {path} ===");
    eprintln!(
        "touching `{target_name}` ({} reinferred/pass) | {iters} iters ({warmup} warmup)\n",
        last_reinferred
    );
    eprintln!(
        "  backend route per pass [ms] (update + diagnostics + lints + overlay):  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}  stddev {:.3} (cv {:.1}%)",
        mean, pct(50.0), pct(99.0), sorted[0], sorted[sorted.len() - 1], stddev, cv
    );
    if write_disk {
        let mut disk_sorted = disk_samples.clone();
        disk_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let disk_mean = disk_samples.iter().sum::<f64>() / n;
        let disk_var = disk_samples.iter().map(|s| (s - disk_mean).powi(2)).sum::<f64>() / n;
        let disk_pct = |p: f64| {
            disk_sorted[((p / 100.0) * (disk_sorted.len() as f64 - 1.0)).round() as usize]
        };
        eprintln!(
            "  disk-inclusive per pass [ms] (backend + atomic overlay write):  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}  stddev {:.3} (cv {:.1}%)",
            disk_mean,
            disk_pct(50.0),
            disk_pct(99.0),
            disk_sorted[0],
            disk_sorted[disk_sorted.len() - 1],
            disk_var.sqrt(),
            if disk_mean > 0.0 { disk_var.sqrt() / disk_mean * 100.0 } else { 0.0 },
        );
    }
    eprintln!("\n  last-pass breakdown (the two-file split path):");
    eprintln!(
        "    Session update_file:            {last_update:.3} ms  (re-lower+re-infer ONLY the {last_reinferred} changed body/bodies)"
    );
    for (lbl, d) in &last_update_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    let update_known: f64 = last_update_phases.iter().map(|(_, d)| d.as_secs_f64() * 1000.0).sum();
    eprintln!("       (parse + classify, untimed: {:.3} ms)", (last_update - update_known).max(0.0));
    eprintln!("    diagnostics (O(workspace) scan): {last_diag:.3} ms");
    eprintln!("    lints (whole-project verdict):  {last_lint:.3} ms");
    eprintln!("    emit_overlay:                   {last_emit:.3} ms  ({last_bytes} B overlay)");
    if write_disk {
        eprintln!("    incremental file write:         {last_write:.3} ms");
    }
    for (lbl, d) in &last_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    if std::env::var_os("SKULL_REPAIR_STATS").is_some() {
        let stats = cache.repair_stats();
        eprintln!(
            "    body metadata: semantic={} span_only={} edit_at={} byte_delta={}",
            last_body_meta.0, last_body_meta.1, last_body_meta.2, last_body_meta.3
        );
        eprintln!(
            "    repair counters: body={} owners={} instances={} graph_retract={} graph_install={} full_graph={} \
concrete_derived={} concrete_reused={} concrete_retracted={} full_concrete={} \
artifact_revisited={} artifact_added={} artifact_removed={} full_artifact={} \
broad_owner={} broad_concrete={} broad_artifact={} broad_candidate={}",
            stats.body_only_edits,
            stats.owners_revisited,
            stats.instances_revisited,
            stats.graph_contributions_retracted,
            stats.graph_contributions_installed,
            stats.full_graph_rebuilds,
            stats.concrete_entries_derived,
            stats.concrete_entries_reused,
            stats.concrete_entries_retracted,
            stats.full_concrete_rebuilds,
            stats.artifact_chunks_revisited,
            stats.artifact_chunks_added,
            stats.artifact_chunks_removed,
            stats.full_artifact_view_realizations,
            stats.broad_owner_scans,
            stats.broad_concrete_scans,
            stats.broad_artifact_scans,
            stats.broad_candidate_scans,
        );
        eprintln!(
            "    locality census: roots requested={} modules_examined={} functions_examined={} reseeded={} root-instances-checked={} \
shader requests={} roots examined={} derived={} declarations requested={} instances looked up={} \
candidates requested={} invalidated={} derived={} reused={} dependents requested={} visited={} \
chunks requested={} rebuilt={} artifact requests={} iterations={} index-rows={} root entries={} realizes={} \
root-hole names={} rows={} decls={} reason-buckets={} erased-catalogue={}",
            stats.root_requests,
            stats.root_modules_examined,
            stats.root_functions_examined,
            stats.root_reseeds,
            stats.root_instances_checked,
            stats.shader_delta_requests,
            stats.shader_roots_examined,
            stats.shader_roots_derived,
            stats.declaration_lookup_requests,
            stats.declaration_instance_lookups,
            stats.candidate_entries_requested,
            stats.candidate_entries_invalidated,
            stats.candidate_entries_derived,
            stats.candidate_entries_reused,
            stats.dependent_requests,
            stats.dependents_visited,
            stats.chunk_rebuild_requests,
            stats.chunks_rebuilt,
            stats.artifact_iteration_requests,
            stats.artifact_iterations,
            stats.artifact_index_rows_examined,
            stats.artifact_root_entries_examined,
            stats.artifact_realize_invocations,
            stats.root_hole_names_examined,
            stats.root_hole_rows_examined,
            stats.root_hole_decls_examined,
            stats.root_hole_reason_buckets_examined,
            stats.erased_catalogue_entries_examined,
        );
    }
    if let Some(census) = crate::lint_pass::take_census() {
        crate::lint_pass::print_census(&census);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stone --backend skull: two-file incremental output (main.lua + incremental.lua)
// ---------------------------------------------------------------------------

fn configured_stone_dir(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some((config_path, config)) = rock_host::stick::StickConfig::find_from_dir(&path)? else {
        return Ok(None);
    };
    if !config.dependencies.contains_key("stone") {
        return Ok(None);
    }
    let manifest_dir = config_path.parent().unwrap_or(&config_path);
    let graph = rock_host::stick::graph::PackageGraph::build(manifest_dir)?;
    let root_node = graph.package(graph.root());
    let edge = root_node
        .dependencies
        .iter()
        .find(|edge| edge.declared_key == "stone")
        .ok_or_else(|| anyhow::anyhow!("Stone dependency is declared but could not be resolved"))?;
    let stone_root = graph.package(edge.target).root.clone();
    Ok(Some(stone_root))
}

fn prepend_stone_root(lua: String, stone_dir: &std::path::Path) -> String {
    let escaped = stone_dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!("__ROCK_STONE_DIR = \"{escaped}\"\n{lua}")
}

/// The game preamble injected at the top of `main.lua`: `GAME_SOURCE_DIR` for
/// asset resolution. cimgui is initialized by stone's global initializer.
/// Factored so `skull_run_project` and the stone two-file path build it
/// identically. `callee_binding` is the shape the chunk beneath it was emitted
/// with -- the SESSION's, not the process's, because one process can hold two
/// sessions of different shapes.
fn skull_game_preamble_for(
    dir: &str,
    callee_binding: skull::CalleeBindingMode,
) -> Result<String> {
    use std::path::PathBuf;
    let abs = PathBuf::from(dir).canonicalize().unwrap_or_else(|_| PathBuf::from(dir));
    let escaped = abs.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let config = match rock_host::stick::StickConfig::find_from_dir(&abs)? {
        Some((_, config)) => Some(config),
        None => None,
    };
    let overlay_defaults = rock_host::release::configured_asset_overlays_from(
        &abs,
        config.as_ref(),
        std::env::var_os(rock_host::release::ASSET_OVERLAYS_ENV).as_deref(),
    )?;
    let renderer = match skull_renderer_target(dir)? {
        skull::mir_emit::RendererTarget::Gl => "gl",
        skull::mir_emit::RendererTarget::Wgpu => "wgpu",
    };
    let mut full = format!("__ROCK_GAME_SOURCE_DIR = \"{escaped}\"\n");
    // The emitted callee shape, for the engine's one-line boot notice about
    // reload granularity (`check_hot_reload` in `stone/engine/hot_reload.rock`).
    // Line 1 stays the source dir -- `read_chunk_source_dir` reads exactly that
    // line to decide whether a chunk may be loaded over the running game.
    if callee_binding == skull::CalleeBindingMode::Upvalue {
        full.push_str("__ROCK_CALLEE_BINDING = \"upvalue\"\n");
    }
    if let Some(stone_dir) = configured_stone_dir(&abs)? {
        let escaped_stone = stone_dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        full.push_str(&format!("__ROCK_STONE_DIR = \"{escaped_stone}\"\n"));
    }
    full.push_str("__ROCK_ASSET_OVERLAY_DEFAULTS = {");
    for (index, path) in overlay_defaults.iter().enumerate() {
        if index > 0 {
            full.push_str(", ");
        }
        let path = path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        full.push('"');
        full.push_str(&path);
        full.push('"');
    }
    full.push_str("}\n");
    // Stamped for EVERY renderer, not only wgpu. The engine reads this global
    // per draw (`stone_renderer_is_wgpu` and friends compare it to "wgpu"),
    // and a global whose value is nil is ABSENT from `_G`, so every read of it
    // fell through to the state bridge's `__index` metamethod: 5.5M dispatches
    // per bench run, 98% of everything that reached the bridge. A present
    // value is a raw hit that constant-folds on a trace.
    full.push_str(&format!("__ROCK_RENDERER_BACKEND = \"{renderer}\"\n"));
    if !full.ends_with('\n') {
        full.push('\n');
    }
    Ok(full)
}

/// [`skull_game_preamble_for`] at the PROCESS shape, for the one-shot paths
/// (`rock run`, `rock compile`) that emit and exit without a build session.
fn skull_game_preamble(dir: &str) -> Result<String> {
    skull_game_preamble_for(dir, skull::callee_binding_mode())
}

pub(crate) fn ensure_renderer_feature(dir: &str) -> Result<()> {
    skull_renderer_target(dir).map(|_| ())
}

fn skull_renderer_target(dir: &str) -> Result<skull::mir_emit::RendererTarget> {
    let abs = std::path::Path::new(dir)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(dir));
    let backend = match rock_host::stick::StickConfig::find_from_dir(&abs)? {
        Some((_, cfg)) => cfg.renderer_backend()?,
        None => rock_host::stick::RendererBackend::Gl,
    };
    if matches!(backend, rock_host::stick::RendererBackend::Wgpu)
        && !cfg!(feature = "renderer-wgpu")
    {
        anyhow::bail!(
            "the `wgpu` renderer was omitted from this build; enable the `renderer-wgpu` feature (for example, `just test-wgpu` or `cargo run --features renderer-wgpu --bin rock ... --renderer wgpu)"
        );
    }
    Ok(match backend {
        rock_host::stick::RendererBackend::Gl => skull::mir_emit::RendererTarget::Gl,
        rock_host::stick::RendererBackend::Wgpu => skull::mir_emit::RendererTarget::Wgpu,
    })
}

/// Game-shaped `main.lua`: preamble + emit + `__rock_main_wrapper(main)` entry.
///
/// `emits_main` comes from the emitter, not from searching `emit_lua`: the
/// emitted spelling of a definition is the emitter's business and has already
/// changed once (`ROCK_SKULL_CALLEE_BINDING=upvalue` moves every body into a
/// factory, so `function main(` appears nowhere), which left a chunk that
/// defined the whole game and then ran nothing.
fn skull_game_main(preamble: &str, emit_lua: &str, emits_main: bool) -> String {
    let mut full = String::from(preamble);
    full.push_str(emit_lua);
    if emits_main {
        full.push_str("\n__rock_main_wrapper(main)\n");
    }
    full
}

/// Remove the accumulated overlay. A missing file is the normal "no overlay yet"
/// case and is fine; ANY other error is surfaced -- a stale `incremental.lua`
/// left beside a fresh `main.lua` would reintroduce the overlay/main desync the
/// two-file split exists to avoid.
pub(crate) fn wipe_overlay(incremental_path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(incremental_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "skull: failed to remove stale overlay {}: {e}",
            incremental_path.display()
        )),
    }
}

/// Write the full game-shaped `main.lua` and wipe any accumulated overlay -- the
/// shared tail of every FULL (re)build: `init`, the interface-rebuild branch of
/// `recompile`, and `force_full`. Returns the bytes written (the size the `Full`
/// reload reports).
fn write_full_main(
    preamble: &str,
    emit_lua: &str,
    emits_main: bool,
    main_path: &std::path::Path,
    incremental_path: &std::path::Path,
) -> Result<usize> {
    let main = skull_game_main(preamble, emit_lua, emits_main);
    let bytes = main.len();
    rock_host::project::write_atomic(main_path, &main)?;
    wipe_overlay(incremental_path)?;
    Ok(bytes)
}

/// What a stone-skull recompile produced. `cook_ms` is the front end (source
/// re-read + per-file incremental cook + re-typecheck via `Session.update_file`);
/// `emit_ms` is codegen + file write (`emit_overlay`/`emit_full`). `bytes` is the
/// written output size -- for `Incremental` that's the overlay size, the headline
/// signal for the overlay-size question.
#[derive(Debug)]
pub enum SkullReload {
    /// Full rebuild: `main.lua` rewritten, `incremental.lua` wiped (interface/
    /// structural edit, or initial build). `body_only` marks the one case the
    /// running process can absorb WITHOUT a restart: a body-tier edit the bound
    /// callee shape forced onto this path (see [`SkullStone::route_and_emit`]).
    /// Nothing structural moved, so the engine's `check_hot_reload` full reload
    /// keeps live state; an interface edit (`body_only: false`) still needs the
    /// clean restart.
    Full { bytes: usize, body_only: bool, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// Body edit: only `incremental.lua` rewritten (the overlay). `reinferred` is
    /// the number of bodies this edit re-inferred (the edit's churn). `emit_ms` is
    /// pure `emit_overlay` (directly comparable to `iter-incremental`); `write_ms`
    /// is the separate disk write of the overlay.
    Incremental { bytes: usize, reinferred: usize, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// A full artifact is required before the current process can observe this
    /// update; no output was written on this path.
    FullRequired { reasons: Vec<String> },
    /// A type's runtime SHAPE changed (docs/hot-reload-schema-drift-plan.md):
    /// live state cannot be migrated in place. Nothing was written -- the
    /// supervisor must use the restart fallback. `types` names the drifted
    /// types for the
    /// why-log.
    ShapeChanged { types: Vec<String> },
    /// Nothing changed (content-hash no-op).
    NoChange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkullRouteProfile {
    pub session_snapshot_ns: u128,
    pub source_lookup_feed_ns: u128,
    pub session_update_ns: u128,
    pub diagnostics_ns: u128,
    pub lint_total_ns: u128,
    pub local_lint_derivation_ns: u128,
    pub global_lint_dependency_ns: u128,
    pub global_lint_derivation_ns: u128,
    pub suppression_filtering_ns: u128,
    pub suppression_filter_calls: usize,
    pub report_prepare_ns: u128,
    pub summary_sort_ns: u128,
    pub render_ns: u128,
    pub finding_aggregation_ns: u128,
    pub publication_shape_checks_ns: u128,
    pub transaction_checkpoint_ns: u128,
    pub body_transactions_started: usize,
    pub transaction_commit_ns: u128,
    pub affected_chunk_selection_ns: u128,
    pub emit_overlay_ns: u128,
    pub atomic_output_write_ns: u128,
    pub rollback_ns: u128,
    pub live_update_ns: u128,
    pub live_project_call_ns: u128,
    pub route_emit_call_ns: u128,
    pub live_project_unaccounted_ns: u128,
    pub route_unaccounted_ns: u128,
    pub total_ns: u128,
    pub unaccounted_ns: u128,
}

impl SkullRouteProfile {
    fn from_live(timing: crate::live_project::LiveRouteTiming) -> Self {
        Self {
            session_snapshot_ns: timing.session_snapshot_ns,
            source_lookup_feed_ns: timing.source_lookup_feed_ns,
            session_update_ns: timing.session_update_ns,
            diagnostics_ns: timing.diagnostics_ns,
            lint_total_ns: timing.lint_total_ns,
            local_lint_derivation_ns: timing.local_lint_derivation_ns,
            global_lint_dependency_ns: timing.global_lint_dependency_ns,
            global_lint_derivation_ns: timing.global_lint_derivation_ns,
            suppression_filtering_ns: timing.suppression_filtering_ns,
            suppression_filter_calls: timing.suppression_filter_calls,
            report_prepare_ns: timing.report_prepare_ns,
            summary_sort_ns: timing.summary_sort_ns,
            render_ns: timing.render_ns,
            finding_aggregation_ns: timing.finding_aggregation_ns,
            rollback_ns: timing.rollback_ns,
            live_update_ns: timing.live_update_ns,
            ..Self::default()
        }
    }

    fn finish(&mut self, total_ns: u128) {
        self.total_ns = total_ns;
        self.live_project_unaccounted_ns = self
            .live_project_call_ns
            .saturating_sub(self.session_snapshot_ns + self.live_update_ns);
        self.route_unaccounted_ns = self
            .route_emit_call_ns
            .saturating_sub(
                self.publication_shape_checks_ns
                    + self.transaction_checkpoint_ns
                    + self.transaction_commit_ns
                    + self.emit_overlay_ns
                    + self.atomic_output_write_ns,
            );
        let accounted = self.live_project_call_ns + self.route_emit_call_ns + self.rollback_ns;
        self.unaccounted_ns = total_ns.saturating_sub(accounted);
    }

    fn add_live(&mut self, timing: crate::live_project::LiveRouteTiming) {
        self.session_snapshot_ns += timing.session_snapshot_ns;
        self.source_lookup_feed_ns += timing.source_lookup_feed_ns;
        self.session_update_ns += timing.session_update_ns;
        self.diagnostics_ns += timing.diagnostics_ns;
        self.lint_total_ns += timing.lint_total_ns;
        self.local_lint_derivation_ns += timing.local_lint_derivation_ns;
        self.global_lint_dependency_ns += timing.global_lint_dependency_ns;
        self.global_lint_derivation_ns += timing.global_lint_derivation_ns;
        self.suppression_filtering_ns += timing.suppression_filtering_ns;
        self.suppression_filter_calls += timing.suppression_filter_calls;
        self.report_prepare_ns += timing.report_prepare_ns;
        self.summary_sort_ns += timing.summary_sort_ns;
        self.render_ns += timing.render_ns;
        self.finding_aggregation_ns += timing.finding_aggregation_ns;
        self.rollback_ns += timing.rollback_ns;
        self.live_update_ns += timing.live_update_ns;
    }
}

fn emit_phase_ns(phases: &[(&'static str, Duration)], label: &str) -> u128 {
    phases
        .iter()
        .find(|(name, _)| *name == label)
        .map_or(0, |(_, duration)| duration.as_nanos())
}

/// Persistent stone-skull compile state: a warm bone `Session` + skull emit cache
/// driving the two-file split (`docs/skull-incremental-emit-split.md`). A body
/// edit re-cooks just the edited file and writes a small `incremental.lua` the
/// running game light-reloads; an interface/structural edit rewrites `main.lua`
/// (full) and wipes the overlay.
pub struct SkullStone {
    project: crate::live_project::LiveProject,
    cache: skull::mir_emit::SkullEmitCache,
    preamble: String,
    renderer_target: skull::mir_emit::RendererTarget,
    /// Per-type layout hashes of the last program the game process could have
    /// loaded. A full recompile whose hashes drifted returns
    /// `SkullReload::ShapeChanged` (full-reconstruction request) instead of writing
    /// (docs/hot-reload-schema-drift-plan.md).
    shape_hashes: skull::ShapeHashes,
    /// The emitted callee shape THIS session builds with. Read instead of the
    /// process shape everywhere a decision has to agree with the bodies already
    /// on disk: the sim runner holds two sessions of different shapes in one
    /// process (the hot-reload lane needs the global-lookup spelling so its
    /// overlay can rebind), and the process pin is one-way and cannot say so.
    callee_binding: skull::CalleeBindingMode,
    route_profile_enabled: bool,
    last_route_profile: Option<SkullRouteProfile>,
    artifact_fingerprint: String,
}

impl SkullStone {
    /// Initial build: warm the Session, `emit_full` -> game-shaped `main.lua`,
    /// wipe `incremental.lua`. Errors render against the project sources.
    /// Game build: excludes test-only files (tests/ dirs) so a broken sim/test
    /// can't break the running game. See `docs/sim-layering.md`.
    pub fn init(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, false, None, None)
    }

    /// Sim-runner build: same as [`SkullStone::init`] but discovery KEEPS
    /// test-only files -- `@sim` bodies live in `tests/sims/` and must be part
    /// of the emitted program.
    pub fn init_with_tests(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, true, None, None)
    }

    /// [`SkullStone::init_with_tests`] under an explicit emitted callee shape.
    ///
    /// The seam the sim runner's hot-reload lane uses. `reload_edit` injection
    /// republishes through [`SkullStone::apply_edit`], which can only produce an
    /// `incremental.lua` overlay under the global-lookup shape, so that lane
    /// compiles its own session with `Some(CalleeBindingMode::Global)` into its
    /// own output directory while every other sim boots the shipped shape from
    /// the shared one. `None` reads the process shape.
    pub fn init_with_tests_and_callee_binding(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        callee_binding: Option<skull::CalleeBindingMode>,
    ) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, true, None, callee_binding)
    }

    /// The emitted callee shape this session builds with.
    pub fn callee_binding(&self) -> skull::CalleeBindingMode {
        self.callee_binding
    }

    pub fn init_with_tests_and_allocation_report(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        allocation_manifest: &std::path::Path,
    ) -> Result<SkullStone> {
        let _activation = skull::allocation_report::activation_scope(true);
        Self::init_impl(
            dir,
            main_path,
            incremental_path,
            true,
            Some(allocation_manifest),
            None,
        )
    }

    fn init_impl(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        include_tests: bool,
        explicit_allocation_report: Option<&std::path::Path>,
        callee_binding: Option<skull::CalleeBindingMode>,
    ) -> Result<SkullStone> {
        let renderer_target = skull_renderer_target(dir)?;
        let project = crate::live_project::LiveProject::init(dir, include_tests)?;
        let (lua, cache, artifact_fingerprint, emits_main) = {
            let ws = project.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                ws,
                mid,
                Some(dir),
                renderer_target,
                callee_binding,
            )
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(project.sources(), &e)))?;
            let allocation_report_path = allocation_report_path_from_env();
            write_allocation_report(
                explicit_allocation_report
                    .map(std::path::Path::as_os_str)
                    .or(allocation_report_path.as_deref()),
                &emit.allocation_report,
            )?;
            // Host package/description metadata is appended to StateSchema.
            let spec = crate::unified_compile::SourceSpec::Directory(
                std::path::PathBuf::from(dir),
            );
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            let mut lua = emit.lua;
            install_rock_state_metadata(&mut lua, ws, &spec)
                .map_err(|e| anyhow::anyhow!(render_project_err(project.sources(), &e)))?;
            (lua, c, artifact_fingerprint, emits_main)
        };
        // The cache answers, not the argument: `None` means "whatever the
        // process resolved", and the preamble stamp plus every reload decision
        // below must name the shape the chunks were actually emitted with.
        let callee_binding = cache.callee_binding();
        let preamble = skull_game_preamble_for(dir, callee_binding)?;
        write_full_main(&preamble, &lua, emits_main, main_path, incremental_path)?;
        let shape_hashes = skull::type_shape_hashes(project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(SkullStone {
            project,
            cache,
            preamble,
            renderer_target,
            shape_hashes,
            callee_binding,
            route_profile_enabled: std::env::var_os("ROCK_SKULL_ROUTE_PROFILE").is_some(),
            last_route_profile: None,
            artifact_fingerprint,
        })
    }

    pub fn enable_route_profile(&mut self) {
        self.route_profile_enabled = true;
        self.project.enable_route_profile();
    }

    pub fn take_last_route_profile(&mut self) -> Option<SkullRouteProfile> {
        self.last_route_profile.take()
    }

    fn finish_route_profile(
        &mut self,
        started: Option<Instant>,
        mut profile: SkullRouteProfile,
    ) {
        if let Some(started) = started {
            profile.finish(started.elapsed().as_nanos());
            self.last_route_profile = Some(profile);
        } else {
            self.last_route_profile = None;
        }
    }

    pub fn artifact_fingerprint(&self) -> &str {
        &self.artifact_fingerprint
    }

    /// On a watch event: refresh the cached sources (re-read only `changed` off
    /// disk, or fully re-discover if the file SET changed), `update_file` every
    /// source (the Session content-hashes unchanged ones to a no-op), then route
    /// by the worst tier -- any interface/structural edit -> full `main.lua` +
    /// wipe overlay; otherwise the accumulated body edits -> `incremental.lua`
    /// overlay. `changed` is the watcher's changed-path set; pass empty to force a
    /// full re-discover.
    pub fn recompile(&mut self, changed: &[std::path::PathBuf], main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.recompile_for_publication(changed);
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                Ok(reload)
            }
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// In-memory edit: replace one source's contents WITHOUT touching disk and
    /// route/emit exactly like a watcher edit. The sim reload-injection path
    /// (`docs/sim-reload-harness-plan.md`): the runner edits a source in memory
    /// and writes only the overlay; the project tree is never mutated. Rejects
    /// a path the build didn't discover -- an edit to an unknown file would
    /// otherwise silently do nothing.
    pub fn apply_edit(&mut self, path: &std::path::Path, new_text: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.apply_edit_for_publication(path, new_text);
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                Ok(reload)
            },
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// The current in-session text of a source (reflects prior `apply_edit`s,
    /// which never touch disk). None if the path isn't a discovered source.
    pub fn source_text(&self, path: &std::path::Path) -> Option<String> {
        self.project.source_text(path)
    }

    /// Absolute paths of every `embed_string` input baked into the current
    /// program -- files the stone watcher must treat as sources: an edit to
    /// one routes as a changed non-source path, which forces the full rebuild
    /// that re-reads it (docs/comptime-plan.md).
    pub fn comptime_watch_files(&self) -> Vec<std::path::PathBuf> {
        // File ids are already the absolute canonical path the resolver read.
        self.cache
            .comptime_files()
            .into_iter()
            .map(|abs| std::path::PathBuf::from(abs.as_str()))
            .collect()
    }

    /// Feed source edits into the Session and route by the worst tier. Body-only
    /// edits write `incremental.lua`; a full artifact is either written or
    /// returned as `FullRequired` when a safety gate defers publication.
    /// Shared by the disk-driven watcher and in-memory sim reload paths.
    /// `t_cook` is the caller's front-end timer (includes its feed construction).
    fn route_and_emit(
        &mut self,
        update: crate::live_project::LiveUpdate,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        mut profile: Option<&mut SkullRouteProfile>,
    ) -> Result<SkullReload> {
        use std::path::PathBuf;
        let (mut full, reinferred, span_edits, cook_ms) = match update {
            crate::live_project::LiveUpdate::NoChange => return Ok(SkullReload::NoChange),
            crate::live_project::LiveUpdate::BodyOnly { reinferred, span_edits, cook_ms } => {
                (false, reinferred, span_edits, cook_ms)
            }
            crate::live_project::LiveUpdate::Full { cook_ms } => {
                (true, Vec::new(), Vec::new(), cook_ms)
            }
        };
        // THE hot-reload rule, read off THIS SESSION's shape rather than the
        // process's -- the sim runner holds two sessions of different shapes at
        // once. A bound caller holds its callee as an immutable upvalue, so an
        // overlay that redefines the callee's GLOBAL would never reach it.
        // Publish the whole chunk instead: both emission passes rerun,
        // `write_full_main` wipes any overlay, and the engine sees a `main.lua`
        // change -- its full reload, which keeps live state. Slower per edit;
        // correct. `assemble_overlay` still rejects a bound body, and under this
        // routing never sees one.
        let bound_body_edit = !full
            && self.callee_binding == skull::CalleeBindingMode::Upvalue;
        full |= bound_body_edit;

        let t_emit = Instant::now();
        let shape_started = profile.is_some().then(Instant::now);
        let old_shader_contracts = self.cache.shader_registration_contracts();
        let shader_keys_changed = if full {
            self.cache
                .shader_registration_keys_changed(self.project.workspace())
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
        } else {
            self.cache
                .shader_registration_keys_changed_for_bodies(
                    self.project.workspace(),
                    &reinferred,
                )
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
        };
        if shader_keys_changed {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            return Ok(SkullReload::FullRequired {
                reasons: vec!["shader registration key set changed".to_string()],
            });
        }
        if full {
            // Schema-drift gate (docs/hot-reload-schema-drift-plan.md): live
            // data outlives every reload tier, so if any type's runtime layout
            // moved, hot-reloading the recompiled program would drive old-shape
            // state with new-shape readers. Request a complete reconstruction
            // and defer ALL writes until the restart path. Re-baseline the stored hashes now: the
            // delta is consumed by this one full publication.
            let new_shapes = skull::type_shape_hashes(self.project.workspace())
                .map_err(|e| anyhow::anyhow!(e.message))?;
            let drifted = skull::shape_diff(&self.shape_hashes, &new_shapes);
            if !drifted.is_empty() {
                self.shape_hashes = new_shapes;
                return Ok(SkullReload::ShapeChanged { types: drifted });
            }
            let lua = {
                let ws = self.project.workspace();
                let mid = skull::user_module_id(ws)
                    .map_err(no_user_module)?;
                let (emit, c) = skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                    ws,
                    mid,
                    Some(self.project.dir()),
                    self.renderer_target,
                    Some(self.callee_binding),
                )
                    .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?;
                if old_shader_contracts != c.shader_registration_contracts() {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec!["shader registration ABI changed".to_string()],
                    });
                }
                if self
                    .cache
                    .state_schema()
                    .is_some_and(|old| {
                        c.state_schema()
                            .is_some_and(|new| old.schema_id != new.schema_id)
                    })
                {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec!["StateSchema changed; fresh Session is required".to_string()],
                    });
                }
                if self.cache.lifecycle_plan().is_some_and(|old| {
                    old.removed_gated_wire(&emit.lifecycle_plan)
                }) {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec![
                            "removing a gated @wire requires a fresh Session/HostProjection".to_string(),
                        ],
                    });
                }
                // StateSchema metadata is regenerated on a structural edit.
                let spec =
                    crate::unified_compile::SourceSpec::Directory(PathBuf::from(self.project.dir()));
                let emits_main = emit.emits_main;
                let mut lua = emit.lua;
                install_rock_state_metadata(&mut lua, ws, &spec)
                    .map_err(|e| anyhow::anyhow!(render_project_err(self.project.sources(), &e)))?;
                ((lua, emits_main), c)
            };
            self.preamble = skull_game_preamble_for(self.project.dir(), self.callee_binding)?;
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            let ((lua, emits_main), staged_cache) = lua;
            let bytes =
                write_full_main(&self.preamble, &lua, emits_main, main_path, incremental_path)?;
            self.cache = staged_cache;
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Full { bytes, body_only: bound_body_edit, cook_ms, emit_ms, write_ms })
        } else {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            let checkpoint_started = profile.is_some().then(Instant::now);
            self.cache
                .begin_body_edit_transaction(&reinferred, &span_edits);
            if let Some(profile) = profile.as_deref_mut() {
                profile.body_transactions_started += 1;
            }
            if let (Some(started), Some(profile)) = (checkpoint_started, profile.as_deref_mut()) {
                profile.transaction_checkpoint_ns += started.elapsed().as_nanos();
            }
            let emit_started = profile.is_some().then(Instant::now);
            let overlay = {
                let ws = self.project.workspace();
                skull::mir_emit::emit_overlay_with_span_edits(
                    ws,
                    &mut self.cache,
                    &reinferred,
                    &span_edits,
                )
                    .map_err(|e| anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e)))?
            };
            if let (Some(started), Some(profile)) = (emit_started, profile.as_deref_mut()) {
                profile.emit_overlay_ns += started.elapsed().as_nanos();
                profile.affected_chunk_selection_ns +=
                    emit_phase_ns(&overlay.phases, "  affected selection");
            }
            if old_shader_contracts != self.cache.shader_registration_contracts() {
                self.cache.commit_body_edit_transaction();
                return Ok(SkullReload::FullRequired {
                    reasons: vec!["shader registration ABI changed".to_string()],
                });
            }
            if self.cache.body_edit_baseline_lifecycle_plan().is_some_and(|old| {
                old.structure_changed(
                    self.cache
                        .lifecycle_plan()
                        .expect("overlay always carries a LifecyclePlan"),
                )
            }) {
                let reason = if self.cache.body_edit_baseline_lifecycle_plan().is_some_and(|old| {
                    old.removed_gated_wire(
                        self.cache
                            .lifecycle_plan()
                            .expect("overlay always carries a LifecyclePlan"),
                    )
                }) {
                    "removing a gated @wire requires a fresh Session/HostProjection"
                } else {
                    "LifecyclePlan structure changed; full artifact transition required"
                };
                self.cache.commit_body_edit_transaction();
                return Ok(SkullReload::FullRequired {
                    reasons: vec![reason.to_string()],
                });
            }
            let bytes = overlay.lua.len();
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            rock_host::project::write_atomic(incremental_path, &overlay.lua)?;
            if let Some(profile) = profile.as_deref_mut() {
                profile.atomic_output_write_ns += t_write.elapsed().as_nanos();
            }
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let commit_started = profile.is_some().then(Instant::now);
            self.cache.commit_body_edit_transaction();
            if let (Some(started), Some(profile)) = (commit_started, profile.as_deref_mut()) {
                profile.transaction_commit_ns += started.elapsed().as_nanos();
            }
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Incremental { bytes, reinferred: reinferred.len(), cook_ms, emit_ms, write_ms })
        }
    }

    /// Canonicalize to a single-file program: `emit_full` from the CURRENT
    /// session workspace -> `main.lua`, and wipe `incremental.lua`. Same effect
    /// as the full-rebuild branch of `recompile`, but driven by a caller rather
    /// than an edit tier and with no re-discovery (the session is already
    /// current).
    ///
    /// Used on the stone restart path: a process restart (libstone/compiler
    /// rebuild) re-spawns luajit, which loads `main.lua` only -- it never
    /// dual-loads the accumulated `incremental.lua` the way the live hot-reload
    /// watcher does. Folding the overlay back into `main.lua` before the restart
    /// means the new process gets the complete current program. The overlay
    /// stays a within-process, single-consumer artifact that never has to cross
    /// a process boundary (`docs/skull-incremental-emit-split.md`).
    pub fn force_full(&mut self, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<()> {
        // A recompile that hit a type error bails AFTER `update_file` has already
        // mutated the session into the broken state, so the restart-path fold must
        // re-gate or it would write a partial `main.lua` to the respawned process.
        self.project.verdict("on restart fold")?;
        let (lua, cache, artifact_fingerprint, emits_main) = {
            let ws = self.project.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::emit_full_in_for_renderer_with_callee_binding(
                ws,
                mid,
                Some(self.project.dir()),
                self.renderer_target,
                Some(self.callee_binding),
            )
                .map_err(|e| anyhow::anyhow!("skull force_full emit: {}", e.message))?;
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            // Host package/description metadata is rebuilt with the StateSchema.
            let spec = crate::unified_compile::SourceSpec::Directory(
                std::path::PathBuf::from(self.project.dir()),
            );
            let mut lua = emit.lua;
            install_rock_state_metadata(&mut lua, ws, &spec)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            (lua, c, artifact_fingerprint, emits_main)
        };
        self.preamble = skull_game_preamble_for(self.project.dir(), self.callee_binding)?;
        write_full_main(&self.preamble, &lua, emits_main, main_path, incremental_path)?;
        self.artifact_fingerprint = artifact_fingerprint;
        self.cache = cache; // resets changed_since_full -- the overlay is now folded in
        // This program is what the (re)spawned process loads -- re-baseline the
        // shape hashes to it, including drift that arrived via a recompile that
        // bailed on a type error after mutating the session.
        self.shape_hashes = skull::type_shape_hashes(self.project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(())
    }
}
