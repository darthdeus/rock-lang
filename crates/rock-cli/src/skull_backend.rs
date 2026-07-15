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
fn skull_emit(file: &str, src: &str) -> std::result::Result<String, Vec<CompilerError>> {
    Ok(skull_emit_full(file, src)?.0)
}

/// Emit the workspace and also discover the user module's `@test` functions
/// (for execution). Returns `(lua, tests)`. `file` is stamped as the source-file
/// id so spans (and `__SITE__`) carry the real path.
fn skull_emit_full(
    file: &str,
    src: &str,
) -> std::result::Result<(String, Vec<skull::SkullTest>), Vec<CompilerError>> {
    let user = skull_parser::parse_module(src, ustr::Ustr::from(file)).map_err(|e| vec![e])?;
    let user_file = user.file;
    let ws = crate::bone_backend::check_with_stdlib([user])?;
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let emit = skull::mir_emit::emit_workspace_mir(&ws, user_mid).map_err(|e| vec![e])?;
    let (lua, skipped) = (emit.lua, emit.skipped);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} stdlib fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }
    let tests = skull::module_tests(&ws, user_mid);
    Ok((lua, tests))
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
    pub emit: Duration,
    /// skull's emit broken into coarse stages (lower / cleanup / emit-lua /
    /// scaffold+misc), from `MirEmit::phases`. Empty if bone rejected the
    /// workspace (emit never ran). Summed element-wise across timed runs.
    pub emit_phases: Vec<(&'static str, Duration)>,
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
        Err(_) => return (false, times),
    };
    times.parse = parse;

    let t = Instant::now();
    // Timed path: assemble via the shared helper but keep the `semantic_pass`
    // call explicit here (the verdict is a bool, not a gate, and the timer must
    // bracket exactly this call).
    let mut modules = crate::bone_backend::with_stdlib(user_modules);
    let ws = match bone_semantic::semantic_passes::semantic_pass(&mut modules) {
        Ok(ws) => ws,
        Err(_) => {
            times.semantic = t.elapsed();
            return (false, times);
        }
    };
    times.semantic = t.elapsed();
    if !crate::bone_backend::body_errors(&ws).is_empty() {
        return (false, times);
    }
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    let t = Instant::now();
    let emit = skull::mir_emit::emit_workspace_mir(&ws, entry_mid);
    times.emit = t.elapsed();
    let emit_ok = match emit {
        Ok(e) => {
            times.emit_phases = e.phases;
            true
        }
        Err(_) => false,
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
            std::process::exit(1);
        }
    };
    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            std::process::exit(1);
        }
    };
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    // Warm-up emit outside the timed loop (catch any gap, prime the allocator).
    if let Err(e) = skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
        eprintln!("{}", render_project_err(&sources, &e));
        std::process::exit(1);
    }

    let t = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..iters {
        match skull::mir_emit::emit_workspace_mir(&ws, entry_mid) {
            Ok(e) => bytes = e.lua.len(),
            Err(e) => {
                eprintln!("{}", render_project_err(&sources, &e));
                std::process::exit(1);
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
            std::process::exit(1);
        }
    };
    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            std::process::exit(1);
        }
    };

    let r = skull::inline_stats::report(&ws);
    let out = "skull-inline-stats.txt";
    std::fs::write(out, &r.text).map_err(|e| anyhow::anyhow!("writing {out}: {e}"))?;

    println!("{} ({path})", "skull inline stats".bold());
    println!(
        "  {} fns scanned, {} inlined into, {} splices, {} inst-copies",
        r.funcs_scanned, r.callers_inlined, r.total_splices, r.total_copies
    );
    println!(
        "  {} distinct callees ({} @inline-forced, {} over the size cap)",
        r.distinct_callees, r.forced, r.over_cap
    );
    if let Some((name, body, times, copies)) = &r.top_callee {
        println!("  most-copied callee: {name} ({body} insts x {times} = {copies} copies)");
    }
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
            std::process::exit(1);
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
fn skull_assemble_project(
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
        // `f` is the global test fn by name; `out` is the expected stdout (or nil).
        // `name` labels the FAIL line (a package unit runs many tests together).
        h.push_str(&format!("  {{ f = {}, out = {}, name = {} }},\n", t.name, out, lua_str(&t.name)));
    }
    h.push_str("}\n");
    h.push_str(
        r#"local function __esc(s) return (s:gsub("\\", "\\\\"):gsub("\n", "\\n"):gsub("\t", "\\t")) end
for _, t in ipairs(__skull_T) do
  local buf = {}
  local rp = print
  local rw = io.write
  -- Model real stdout byte-faithfully: `print` joins args with TAB and adds a
  -- trailing newline; `io.write` appends raw fragments. One accumulator, one
  -- trailing newline stripped at the end (matches how @test(output=..) expects).
  print = function(...)
    local p = {}
    for i = 1, select('#', ...) do p[i] = tostring((select(i, ...))) end
    buf[#buf + 1] = table.concat(p, "\t") .. "\n"
  end
  io.write = function(...)
    for i = 1, select('#', ...) do buf[#buf + 1] = tostring((select(i, ...))) end
  end
  local ok, err = pcall(t.f)
  print = rp
  io.write = rw
  local got = table.concat(buf, ""):gsub("^%s+", ""):gsub("%s+$", "")
  if not ok then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": error: ", __esc(tostring(err)), "\n")
  elseif t.out ~= nil and got ~= t.out then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": output mismatch: expected <", __esc(t.out), "> got <", __esc(got), ">\n")
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
fn render_file_errs(file: &str, src: &str, errs: &[CompilerError]) -> String {
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
    let (lua, tests) = match skull_emit_full(file, src) {
        Ok(v) => v,
        Err(errs) => {
            // A file whose `@test`s ALL expect failure is supposed to fail to
            // compile — a bone rejection here is the expected (passing) outcome.
            // (Mirrors the real rock runner's compile-Err branch.)
            let mut sf = SourceFiles { files: vec![] };
            sf.add_or_update_file(source_file_from_path_and_contents(
                file.to_string(),
                file.to_string(),
                src.to_string(),
            ));
            let test_fns = crate::test::find_test_functions_from_sources(&sf);
            if !test_fns.is_empty() && test_fns.iter().all(|t| t.expect_failure || t.ignored) {
                // Every `@test` expected the compile to fail, and it did: each
                // counts as a passing test.
                return Ok(SkullOutcome::Pass { tests: test_fns.len() });
            }
            return Err(SkullFailure {
                reason: categorize(&errs[0].message),
                rendered: render_file_errs(file, src, &errs),
            });
        }
    };

    run_emitted_tests(&lua, &tests)
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
            Ok(out) if !out.contains("STDERR:") => return Ok(SkullOutcome::Pass { tests: 0 }),
            Ok(out) => {
                return Err(SkullFailure {
                    reason: "invalid lua emitted (skull bug)".to_string(),
                    rendered: format!("{out}\n\n--- emitted lua (truncated) ---\n{}", truncate(lua)),
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
    let stdout = match rock_host::lua_runner::run_lua_code_with_timeout(&program, timeout) {
        Ok(s) => s,
        Err(e) => {
            return Err(SkullFailure {
                reason: "test run errored (skull bug)".to_string(),
                rendered: e.message,
            });
        }
    };

    let results: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("__SKULL__\t"))
        .collect();

    // No result lines at all => the module didn't even load (a skull bug),
    // not a test failure.
    if results.is_empty() {
        return Err(SkullFailure {
            reason: "invalid lua emitted (skull bug)".to_string(),
            rendered: format!("{stdout}\n\n--- emitted lua (truncated) ---\n{}", truncate(&program)),
        });
    }

    let fails: Vec<String> = results
        .iter()
        .filter(|l| l.contains("\tFAIL\t"))
        .map(|l| l.splitn(3, '\t').nth(2).unwrap_or("").to_string())
        .collect();

    let passed = results.len() - fails.len();
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
    let ws = crate::bone_backend::check_modules(&mut modules).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
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
    let tests = collect_package_tests(&ws, dir);
    run_emitted_tests(&lua, &tests)
}

/// The `@test` fns defined in the package rooted at `dir` -- its own source
/// files and its `tests/`, but NOT stdlib or dependency packages (which live at
/// other canonical paths). Module file paths are canonicalized absolute, so a
/// prefix match against the canonicalized `dir` selects exactly this package.
fn collect_package_tests(
    ws: &bone_semantic::inference::Workspace,
    dir: &str,
) -> Vec<skull::SkullTest> {
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
            out.extend(skull::module_tests(ws, m.id));
        }
    }
    out
}

/// Keep emitted-Lua dumps in error reports manageable.
fn truncate(s: &str) -> String {
    if s.len() > 4000 {
        format!("{}\n... ({} more bytes)", &s[..4000], s.len() - 4000)
    } else {
        s.to_string()
    }
}

/// `rock compile <file> --backend skull` — emit Lua to `output` (or stdout).
/// `rock shader <file.rock> <fn> [fn...] --output <file.glsl>` -- GLSL
/// function-library emission (route B, `docs/glsl-fn-emission.md`). Type-checks
/// the file against the stdlib, then emits GLSL fn definitions for the named root
/// fns + their reachable tier-0 callees. Backend-agnostic (dispatched before the
/// backend switch). A profile violation renders spanned against the source and
/// exits non-zero.
pub fn skull_shader(files: &[String], output: Option<String>) -> Result<()> {
    let Some((path, roots)) = files.split_first() else {
        anyhow::bail!("rock shader: usage: rock shader <file.rock> <fn> [fn...] --output <file.glsl>");
    };
    if roots.is_empty() {
        anyhow::bail!("rock shader: need at least one root fn name");
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SHADER", "cannot read {}: {}", path, e);
            std::process::exit(1);
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(path.as_str())) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}", render_file_errs(path, &src, &[e]));
            std::process::exit(1);
        }
    };
    let user_file = user.file;
    let ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            std::process::exit(1);
        }
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let roots: Vec<String> = roots.to_vec();
    let glsl = match skull_codegen_glsl::emit_shader(&ws, user_mid, &roots) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{}", render_file_errs(path, &src, &[e]));
            std::process::exit(1);
        }
    };
    match output {
        Some(out) => {
            // Keep the fn-library header text stable: utils_math.glsl byte-identity.
            let mode = if glsl.starts_with("#version") { "shader entry" } else { "route B" };
            let header = format!(
                "// GENERATED by skull-codegen-glsl ({mode}) -- DO NOT EDIT.\n\
                 // Regenerate: rock shader {} {} --output {}\n\n",
                path,
                roots.join(" "),
                out
            );
            rock_host::project::write_atomic(&out, &format!("{header}{glsl}"))?;
            log::info!(target: "SHADER", "{} [{}] -> {}", path, roots.join(", "), out);
        }
        None => print!("{glsl}"),
    }
    Ok(())
}

pub fn skull_compile_single(path: &str, output: Option<String>) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_compile_project(path, output);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            std::process::exit(1);
        }
    };
    match skull_emit(path, &src) {
        Ok(lua) => {
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
            std::process::exit(1);
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
        // is_game gate. See `docs/sims-out-of-game-build-plan.md`.
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
            None => match crate::bone_backend::bone_session(files.clone()) {
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
            std::process::exit(1);
        }
    };
    let lua = match skull_emit(path, &src) {
        Ok(l) => l,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            std::process::exit(1);
        }
    };
    let program = if lua.contains("function main(") {
        format!("{lua}\nmain()\n")
    } else {
        lua
    };
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(output) => {
            print!("{}", output);
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            std::process::exit(1);
        }
    }
}

/// Render a bone `CompilerError` against a whole project's source set, so spans
/// resolve to the right file. Every discovered source is registered under its
/// id-path (the same id used when parsing into a module).
fn render_project_err(sources: &SourceFiles, e: &CompilerError) -> String {
    render_project_errs(sources, std::slice::from_ref(e))
}

/// [`render_project_err`] over every collected diagnostic, joined into one
/// report — the CLI prints ALL of them, matching the LSP.
fn render_project_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
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
/// loop): the `is_game` branch below stages libstone and the cimgui prelude and
/// execs luajit on `tmp/main.lua`, reusing the rock backend's run recipe
/// (`build_engine_libs`, `exec_luajit_file`). All games run this way (match3,
/// mag, bitgun, ...). Non-game projects keep the captured-output pipe. See
/// `docs/skull-plan.md` — whole-game milestone (reached).
/// Discover + assemble + bone-typecheck + skull-emit a whole project directory
/// into one Lua program string. The shared core of `run <dir>` and `compile
/// <dir>` (the single-file forms share [`skull_emit_full`]); only the tail
/// differs — `run` wraps `main()` and execs, `compile` writes the string out.
/// Renders against the project sources and exits 1 on any discovery / type /
/// emit failure.
fn skull_emit_project(dir: &str) -> String {
    use std::path::PathBuf;
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot load project {}: {}", dir, e);
            std::process::exit(1);
        }
    };
    // Game build: drop test-only files (tests/ dirs) so a compile error in a
    // sim/test can't break the running game. See
    // `docs/sims-out-of-game-build-plan.md`.
    let sources = sources.production_only();

    // Compile core: parse(assemble) + bone check + skull codegen. Excludes
    // discovery (disk read) and the caller's output write, matching the bone
    // backend's `compile` figure so loc/sec lines up across backends.
    let t = Instant::now();
    let (mut modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            std::process::exit(1);
        }
    };

    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            log::error!(target: "SKULL", "bone type-check failed for project {}", dir);
            std::process::exit(1);
        }
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    let (lua, skipped) = match skull::mir_emit::emit_workspace_mir(&ws, user_mid) {
        Ok(e) => (e.lua, e.skipped),
        Err(e) => {
            eprintln!("{}", render_project_err(&sources, &e));
            log::error!(target: "SKULL", "skull emit failed for project {}", dir);
            std::process::exit(1);
        }
    };
    let compile = t.elapsed();
    crate::bone_backend::log_compile_stats(&sources, &ws, compile, compile, None);
    if std::env::var("SKULL_DEBUG_SKIPPED").is_ok() && !skipped.is_empty() {
        eprintln!("skull: {} fns skipped", skipped.len());
        for (name, reason) in &skipped {
            eprintln!("  {name}: {reason}");
        }
    }

    // `_G.__ROCK_GLOBALS` registry (consumed by `--dump-state` /
    // `dump_globals_json`). bone/skull are package-agnostic, so the
    // package grouping is computed here at the host layer (where stick.toml
    // lives), mirroring rock-codegen's `format_globals_registry`. Appended
    // after the emit; the block only lists global *names* (read from `_G`
    // at runtime), so position past the global defs is irrelevant.
    let registry = build_rock_globals_registry(&ws, &spec);
    let lua = format!("{lua}{registry}");
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
    lua
}

/// Build the `_G.__ROCK_GLOBALS = {["pkg"]={"name",...},...}` registry that
/// `--dump-state` / `dump_globals_json` reads. bone is package-agnostic, so the
/// grouping is done here: every global is attributed to its package by
/// longest-prefix match of its module file against the stick.toml package map
/// (same rule as rock-codegen's assembler). Returns "" when there's no project
/// package map (single-file / no stick.toml), matching rock (no registry then).
fn build_rock_globals_registry(
    ws: &bone_semantic::inference::Workspace,
    spec: &crate::unified_compile::SourceSpec,
) -> String {
    use bone_semantic::prelude::ModuleId;
    use std::collections::BTreeMap;

    let package_map = crate::unified_compile::build_package_map(spec);
    if package_map.is_empty() {
        return String::new();
    }

    let mut by_package: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
        let Some(pkg) = matched else { continue };
        let entry = by_package.entry(pkg.to_string()).or_default();
        for (_gid, g) in m.globals.iter() {
            entry.push(g.name.as_str().to_string());
        }
    }
    by_package.retain(|_, names| !names.is_empty());
    if by_package.is_empty() {
        return String::new();
    }

    // Bracket-string keys so hyphenated package names (`ui-proto`) stay valid
    // Lua (bare-identifier keys would be a syntax error).
    let mut out = String::from("\n_G.__ROCK_GLOBALS = {");
    for (pkg, names) in &by_package {
        out.push_str(&format!("[{pkg:?}]={{"));
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{name:?}"));
        }
        out.push_str("},");
    }
    out.push_str("}\n");
    out
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
            if f.annotations.iter().any(|a| a.name.name.as_str() == "sim_render") {
                env_pairs.push(("SIM_SKIP_RENDER".to_string(), "0".to_string()));
            }
            for ann in &f.annotations {
                if ann.name.name.as_str() != "sim_env" {
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
    // Session-based build (same machinery as `rock stone`, tests included so the
    // `@sim` bodies are in the program). The live SkullStone is returned so the
    // runner can serve mid-sim `reload_edit` ops through the REAL incremental
    // path (`docs/sim-reload-harness-plan.md`).
    let incremental_path = main_path.with_file_name("incremental.lua");
    let stone = SkullStone::init_with_tests(dir, main_path, &incremental_path)
        .map_err(|e| anyhow::anyhow!("cannot compile project {dir}: {e}"))?;

    let sims = collect_skull_sims(stone.session.workspace());
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
    use std::path::PathBuf;
    let emit = skull_emit_project(dir);
    // A game's runnable main.lua is the same shape run/stone produce via
    // write_full_main: preamble (cimgui bindings + game source dir) + emit +
    // __rock_main_wrapper(main). Without the preamble the fire build is missing
    // the imgui bindings (GetStyle etc.); without the wrapper it never starts.
    let lua = if rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false) {
        skull_game_main(&skull_game_preamble(dir), &emit)
    } else if emit.contains("function main(") {
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
    let lua = skull_emit_project(dir);

    // Game projects need the rock backend's run recipe, not a bare stdin
    // pipe: stage libstone into tmp/ (build_engine_libs), run luajit on a
    // FILE with LD_LIBRARY_PATH set (build_luajit_command) so
    // `ffi.load("libstone.so")` resolves, and inherit stdio so the
    // window/input work. Non-game projects keep the captured-output pipe.
    let is_game =
        rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false);
    if is_game {
        if !std::env::var("ROCK_SKIP_ENGINE_BUILD").is_ok_and(|v| v == "1") {
            if let Err(e) = rock_host::engine_build::build_engine_libs(dir) {
                log::error!(target: "SKULL", "engine build failed: {}", e);
                std::process::exit(1);
            }
        }
        // Game preamble + `__rock_main_wrapper(main)` entry, built by the same
        // helpers the stone two-file watch path uses (so both write an
        // identical tmp/main.lua), and wipe any stale overlay so the engine's
        // hot-reload watcher can't load a leftover incremental.lua over the
        // fresh baseline.
        let lua_file = rock_host::project::default_main_lua_path();
        if let Err(e) = write_full_main(
            &skull_game_preamble(dir),
            &lua,
            &lua_file,
            &lua_file.with_file_name("incremental.lua"),
        ) {
            log::error!(target: "SKULL", "cannot write {}: {}", lua_file.display(), e);
            std::process::exit(1);
        }
        // No timeout: games run an infinite frame loop.
        return crate::compile_cmd::exec_luajit_file(&lua_file, None, &[]);
    }

    // Non-game: append the entry wrapper and pipe the captured output.
    let program = if lua.contains("function main(") {
        format!("{lua}\n__rock_main_wrapper(main)\n")
    } else {
        lua
    };
    match rock_host::lua_runner::run_lua_code(&program) {
        Ok(output) => {
            print!("{}", output);
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            std::process::exit(1);
        }
    }
}

/// `rock test <paths> --backend skull` — for every `.rock` file, run the skull
/// pipeline and the luajit validity gate; report pass/fail + a bucketed
/// failure-reason histogram (the skull analogue of the bone test histogram).
pub fn skull_test(paths: &[String]) -> Result<()> {
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();

    // A test UNIT is either an isolated file (root `tests/`: independent
    // programs that would collide in one workspace) or a whole `stick.toml`
    // package built as ONE workspace. A cross-file lib can only be tested the
    // second way -- per-file, any file referencing a sibling's symbol fails to
    // compile. Package dirs go the workspace route; everything else per-file.
    let mut files = Vec::new();
    let mut package_dirs = Vec::new();
    for path in paths {
        if is_test_package_dir(path) {
            package_dirs.push(path.clone());
        } else {
            files.extend(collect_rock_files(path)?);
        }
    }
    files.sort();
    files.dedup();
    package_dirs.sort();
    package_dirs.dedup();

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
    } else if failed > 0 {
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

    if failed > 0 || lsp_failed > 0 || err_failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// luajit run + stdout-normalization helpers, shared by the hot-reload gates below.

/// Per-test execution outcome (clean stdout vs errored), as the gates read it.
#[cfg(test)]
enum ExecOutcome {
    /// Clean run; holds full stdout (module load prints + test prints).
    Output(String),
    /// Runtime error, lua load failure, spawn failure, or timeout.
    Errored,
}

#[cfg(test)]
fn exec_lua(program: &str) -> ExecOutcome {
    let timeout = Some(Duration::from_secs(15));
    match rock_host::lua_runner::run_lua_code_with_timeout(program, timeout) {
        Ok(out) if out.contains("STDERR:") => ExecOutcome::Errored,
        Ok(out) => ExecOutcome::Output(out),
        Err(_) => ExecOutcome::Errored,
    }
}

/// Replace `0x<hex>` sequences with `0xADDR`: table/cdata identity prints
/// differ per-process and would false-positive every diff.
#[cfg(test)]
fn normalize_stdout(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'0' && i + 2 < bytes.len() && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
        {
            out.push_str("0xADDR");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// First line where `lhs` and `rhs` differ, for divergence reporting.
#[cfg(test)]
fn first_diff_line(lhs: &str, rhs: &str) -> String {
    for (i, (a, b)) in lhs.lines().zip(rhs.lines()).enumerate() {
        if a != b {
            return format!("line {}: {:?} != {:?}", i + 1, a, b);
        }
    }
    format!(
        "line count: {} != {}",
        lhs.lines().count(),
        rhs.lines().count()
    )
}

/// `true` when the outputs contain the same lines in a different order.
/// Map iteration rides Lua `pairs()`, whose order is unspecified and can
/// legitimately differ between a frozen baseline+overlay and a fresh build.
#[cfg(test)]
fn is_order_only(a: &str, b: &str) -> bool {
    let mut la: Vec<&str> = a.lines().collect();
    let mut lb: Vec<&str> = b.lines().collect();
    la.sort_unstable();
    lb.sort_unstable();
    la == lb
}

#[cfg(test)]
mod incremental_watch_tests {
    //! End-to-end check that a real `bone_semantic::Session`'s `reinferred` list drives
    //! `emit_incremental` to the same Lua a fresh full build produces — the watch
    //! loop's correctness, minus the file-watching. Mirrors the in-skull oracle
    //! (`skull/tests/incremental_emit.rs`) but through the actual Session edit
    //! path used by `skull_watch`, so it also covers "is `reinferred` enough".

    use crate::bone_backend::bone_session;

    fn files(src: &str) -> Vec<(String, String)> {
        vec![("main.rock".to_string(), src.to_string())]
    }

    #[test]
    fn session_body_edit_incremental_matches_fresh() {
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }";

        // Warm session at v1, full emit -> cache.
        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full_v1, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1");

        // Edit helper's body -> a body-level re-inference.
        let result = session.update_file("main.rock", v2).expect("update");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "editing a fn body should be a BodyOnly update, got {:?}",
            result.tier
        );
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred)
                .expect("emit_incremental");

        // Fresh session at v2, full emit.
        let fresh_session = bone_session(files(v2)).expect("session v2");
        let fresh = skull::mir_emit::emit_full(fresh_session.workspace(), mid).expect("emit_full v2").0;

        assert_eq!(
            incremental.lua, fresh.lua,
            "Session-driven incremental emit != fresh full emit after a body edit"
        );
    }

    #[test]
    fn cross_file_inline_edit_matches_fresh() {
        // a.rock's tiny `helper` is inlined into b.rock's `caller` (a CROSS-MODULE
        // inline dep — `deps` holds the callee's FuncRef across modules). Editing
        // a.rock re-infers only a's body, but skull must still re-emit b's caller.
        let mut session = bone_session(vec![
            ("a.rock".into(), "@inline\nfn helper() -> int { 3 }".into()),
            ("b.rock".into(), "fn caller() -> int { helper() }".into()),
        ])
        .expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_init, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full");

        let result = session.update_file("a.rock", "@inline\nfn helper() -> int { 7 }").expect("update");
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred).expect("inc");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("fresh").0;
        assert_eq!(incremental.lua, fresh.lua, "cross-file inline edit: incremental != fresh");
    }

    /// Byte-identity oracle for the taken_by_ref -> ScalarCtx -> affected-set
    /// interaction. `foo` has a value-struct param: while a body takes it BY
    /// REFERENCE (`let f = foo`) it keeps the single-cdata ABI; with no by-ref it
    /// scalarizes. A body edit that flips that membership must re-emit foo's chunk
    /// AND its direct callers (their call-site shape changes), even though foo's
    /// own body is unchanged. Each case asserts incremental == fresh.
    fn abi_flip_case(v1: &str, v2: &str) {
        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_full, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1");
        let result = session.update_file("main.rock", v2).expect("update");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "edit should be BodyOnly, got {:?}",
            result.tier
        );
        let incremental =
            skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred).expect("inc");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("fresh").0;
        assert_eq!(incremental.lua, fresh.lua, "by-ref ABI flip: incremental != fresh");
    }

    const ABI_DECL: &str = "value V2 { x: float, y: float }\n\
                            fn foo(v: V2) -> float { v.x }\n\
                            fn other() -> float { foo(V2 { x: 2.0, y: 3.0 }) }\n";

    #[test]
    fn taken_by_ref_removal_flips_abi() {
        // use_ref drops its by-ref to foo -> foo scalarizes (cdata -> splat).
        abi_flip_case(
            &format!("{ABI_DECL}fn use_ref() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}"),
            &format!("{ABI_DECL}fn use_ref() -> float {{ other() }}"),
        );
    }

    #[test]
    fn taken_by_ref_addition_flips_abi() {
        // use_ref STARTS taking foo by ref -> foo must flip scalar -> cdata. This
        // is the runtime-dangerous direction: a stale scalarized foo def would get
        // a single cdata from the new CallValue site.
        abi_flip_case(
            &format!("{ABI_DECL}fn use_ref() -> float {{ other() }}"),
            &format!("{ABI_DECL}fn use_ref() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}"),
        );
    }

    #[test]
    fn taken_by_ref_refcount_keeps_abi_when_one_referencer_remains() {
        // TWO bodies take foo by ref; removing ONE must NOT flip foo (still taken
        // by ref by the other) -- the refcount, not a naive set, is load-bearing.
        // A naive set-removal would wrongly scalarize foo here; incremental would
        // then diverge from fresh (which keeps foo as cdata).
        let v1 = format!(
            "{ABI_DECL}fn ref_a() -> float {{ let f = foo; f(V2 {{ x: 1.0, y: 2.0 }}) }}\n\
             fn ref_b() -> float {{ let g = foo; g(V2 {{ x: 3.0, y: 4.0 }}) }}"
        );
        let v2 = format!(
            "{ABI_DECL}fn ref_a() -> float {{ other() }}\n\
             fn ref_b() -> float {{ let g = foo; g(V2 {{ x: 3.0, y: 4.0 }}) }}"
        );
        abi_flip_case(&v1, &v2);
    }

    #[test]
    fn fast_path_multiline_then_following_edit_matches_fresh() {
        // Incremental-parse fast path: edit `a`'s body so it GROWS by lines
        // (line_delta != 0, shifting every following item), then edit `c` (which
        // sits after `a`). The second edit must locate `c` through the metadata
        // shifted by the first. Each step's emit must equal a fresh full build.
        let v0 = "fn a() -> int { 1 }\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
        let v1 = "fn a() -> int {\n    let x = 1\n    x + 10\n}\nfn b() -> int { 2 }\nfn c() -> int { 3 }";
        let v2 = "fn a() -> int {\n    let x = 1\n    x + 10\n}\nfn b() -> int { 2 }\nfn c() -> int { 99 }";

        let mut session = bone_session(files(v0)).expect("v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_f0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit v0");

        let r1 = session.update_file("main.rock", v1).expect("update a");
        let inc1 = skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &r1.reinferred).expect("inc1");
        let fresh1 = skull::mir_emit::emit_full(bone_session(files(v1)).unwrap().workspace(), mid).expect("fresh1").0;
        assert_eq!(inc1.lua, fresh1.lua, "after multi-line growth edit to `a`");

        let r2 = session.update_file("main.rock", v2).expect("update c");
        let inc2 = skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &r2.reinferred).expect("inc2");
        let fresh2 = skull::mir_emit::emit_full(bone_session(files(v2)).unwrap().workspace(), mid).expect("fresh2").0;
        assert_eq!(inc2.lua, fresh2.lua, "after editing `c` following a multi-line edit to `a`");
    }

    // --- fuzz oracle: incremental == fresh over random valid body edits -------
    //
    // The plan is emphatic that hand-picked tests miss codegen-dep undercoverage
    // ("only the incremental==fresh fuzz catches it" — rock's deletion bug). This
    // builds a function call/inline graph, then mutates random bodies (always
    // type-valid by construction: every fn is `-> int`, every callee is a
    // lower-indexed fn so the graph is a DAG, no recursion), driving the REAL
    // `Session.update_file` -> `emit_incremental` path. After each edit it asserts
    // the incremental assembly is byte-identical to `emit_full` of the SAME
    // (edited) workspace — the fresh build is the ground truth. A divergence means
    // a chunk that should have re-emitted (an inline-dependent caller) didn't.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    #[derive(Clone)]
    enum Body {
        Const(i64),
        Call(usize), // calls f{j}, j < i (DAG, inlinable, no recursion)
    }

    fn render(bodies: &[Body]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        // `@inline` on every fn so the call graph actually inlines under the
        // default annotation-gated policy -- this fuzz's whole point is to
        // exercise inline-dependency re-emit (a leaf edit must re-emit its
        // inliners) + InlineCache invalidation across edits.
        for (i, b) in bodies.iter().enumerate() {
            match b {
                Body::Const(k) => {
                    let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ {k} }}");
                }
                Body::Call(j) => {
                    let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ f{j}() }}");
                }
            }
        }
        s
    }

    /// Mutate one body to a new valid form; returns the index changed.
    fn mutate(bodies: &mut [Body], rng: &mut Rng) -> usize {
        let i = rng.pick(bodies.len());
        bodies[i] = if i == 0 || rng.pick(2) == 0 {
            Body::Const(rng.pick(1000) as i64)
        } else {
            Body::Call(rng.pick(i)) // some lower-indexed fn -> inline edge
        };
        i
    }

    fn run_fuzz(seed: u64, n_fns: usize, steps: usize) {
        let mut rng = Rng(seed.max(1));
        // Start: a mix of consts and call edges so inlining is exercised from step 0.
        let mut bodies: Vec<Body> = (0..n_fns)
            .map(|i| {
                if i > 0 && rng.pick(2) == 0 {
                    Body::Call(rng.pick(i))
                } else {
                    Body::Const(rng.pick(1000) as i64)
                }
            })
            .collect();

        let mut session = bone_session(files(&render(&bodies))).expect("fuzz session init");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (_init, mut cache) =
            skull::mir_emit::emit_full(session.workspace(), mid).expect("fuzz emit_full init");

        for step in 0..steps {
            let changed = mutate(&mut bodies, &mut rng);
            let text = render(&bodies);
            let result = session
                .update_file("main.rock", &text)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: update_file failed: {}", e.message));

            // By construction every program type-checks; a diagnostic is a bug.
            let diags = session.diagnostics();
            assert!(
                diags.is_empty(),
                "seed {seed} step {step} (edited f{changed}): unexpected diagnostics: {:?}\n--- program ---\n{text}",
                diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
            );

            let incremental =
                skull::mir_emit::emit_incremental(session.workspace(), &mut cache, &result.reinferred)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_incremental failed: {}", e.message));
            let fresh = skull::mir_emit::emit_full(session.workspace(), mid)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_full failed: {}", e.message))
                .0;

            assert_eq!(
                incremental.lua, fresh.lua,
                "seed {seed} step {step}: incremental != fresh after editing f{changed} \
                 (reinferred={} bodies)\n--- program ---\n{text}",
                result.reinferred.len()
            );
        }
    }

    #[test]
    fn fuzz_incremental_matches_fresh() {
        // A handful of deterministic seeds; each drives many valid body edits on a
        // call/inline graph and checks incremental==fresh every step. Override with
        // SKULL_FUZZ_SEEDS / SKULL_FUZZ_STEPS for a longer soak.
        let steps: usize = std::env::var("SKULL_FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let seeds: u64 = std::env::var("SKULL_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
        for seed in 1..=seeds {
            run_fuzz(seed.wrapping_mul(2654435761), 16, steps);
        }
    }
}

#[cfg(test)]
mod overlay_runtime_tests {
    //! Runtime-equivalence oracle for the TWO-FILE SPLIT (`main.lua` +
    //! `incremental.lua`). The byte-identical oracles (`incremental_watch_tests`,
    //! `skull/tests/incremental_emit.rs`) cover the full-reassembly
    //! `emit_incremental` path; they say nothing about the overlay, which is NOT
    //! byte-equal to a full build by design (Lua later-definition-wins). The only
    //! correctness gate for the split is BEHAVIORAL: a frozen `main.lua` (the
    //! stale baseline) + an accumulating overlay, EXECUTED under luajit, must
    //! print exactly what a fresh full build of the final source prints.
    //!
    //! This exercises three things no other test does at runtime:
    //!   1. later-definition-wins -- the overlay's fresh `foo` overrides the
    //!      stale `foo` baked into the frozen baseline;
    //!   2. accumulation -- edit A then B, the overlay must still carry A' (else A
    //!      silently reverts to the baseline's stale copy);
    //!   3. inline-dependent caller re-emit -- editing an inlined callee must
    //!      re-emit its callers into the overlay (the callers' chunks contain the
    //!      callee's body), or the caller runs the stale inlined value.
    //!
    //! Strategy mirrors `SkullStone`: drive edits through a real `bone_semantic::Session`'s
    //! `update_file` -> `reinferred`, `emit_overlay` from the SAME accumulating
    //! cache, and compose `frozen_baseline ++ overlay` against a fresh `emit_full`.

    use crate::bone_backend::bone_session;
    use super::{exec_lua, normalize_stdout, ExecOutcome};

    /// Standalone `@test`/synthetic programs have no engine, but the overlay
    /// always calls `clear_all_systems()` (the system rebind). Stub it so the
    /// overlay loads; these programs register no systems, so the stub is a
    /// faithful no-op.
    const SYS_STUB: &str = "function clear_all_systems() end\n";

    /// The stub ONLY when the program doesn't define its own `clear_all_systems`.
    /// Files exercising the decorator/system machinery emit a REAL one (+ the
    /// registries); stubbing it as a no-op there would shadow it (later-wins) and
    /// double-register on the rebind. Empty string when a real one is present.
    fn stub_for(full_lua: &str) -> &'static str {
        if full_lua.contains("function clear_all_systems") { "" } else { SYS_STUB }
    }

    /// `main` is unmangled (no-mangle exception), so a bare `main()` entry runs
    /// it on both sides without computing a mangled name. Append AFTER the
    /// overlay so the overlay's fresh `main` wins over the stale baseline's.
    fn run_parts(parts: &[&str]) -> ExecOutcome {
        exec_lua(&parts.join("\n"))
    }

    /// Assert the frozen-baseline+overlay execution matches the fresh full build
    /// byte-for-byte on stdout (addresses normalized). `ctx` labels failures.
    fn assert_runtime_eq(stale_main: &str, overlay: &str, fresh_full: &str, ctx: &str) {
        let composed = run_parts(&[stale_main, stub_for(stale_main), overlay, "main()"]);
        let fresh = run_parts(&[fresh_full, stub_for(fresh_full), "main()"]);
        match (composed, fresh) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                let (nc, nf) = (normalize_stdout(&c), normalize_stdout(&f));
                assert_eq!(
                    nc, nf,
                    "{ctx}: frozen-baseline + overlay diverged from fresh full build at runtime\n\
                     --- composed ---\n{nc}\n--- fresh ---\n{nf}"
                );
            }
            (ExecOutcome::Errored, ExecOutcome::Errored) => {
                panic!("{ctx}: BOTH sides errored -- harness bug (bad source or missing stub)");
            }
            (ExecOutcome::Output(_), ExecOutcome::Errored) => {
                panic!("{ctx}: composed (baseline+overlay) runs but fresh full build errors")
            }
            (ExecOutcome::Errored, ExecOutcome::Output(_)) => {
                panic!("{ctx}: fresh full build runs but composed (baseline+overlay) errors")
            }
        }
    }

    fn files(src: &str) -> Vec<(String, String)> {
        vec![("main.rock".to_string(), src.to_string())]
    }

    /// The skull watcher's robustness contract: a BROKEN edit between two valid
    /// states must (a) surface diagnostics so the watcher skips the hot reload
    /// and keeps the last-good program running, and (b) leave the session able to
    /// recover -- the next valid edit emits a program identical to a fresh build.
    /// The parse is total, so a broken edit no longer fails `update_file`; the
    /// watcher gates on `diagnostics()` instead and drops its incremental cache so
    /// recovery is a correct full rebuild (not a diff across the broken state).
    #[test]
    fn watcher_skips_reload_on_broken_edit_and_recovers() {
        let v1 = "fn compute() -> int { 1 }\nfn main() { print(\"{compute()}\") }\n";
        let v2 = "fn compute() -> int { 42 }\nfn main() { print(\"{compute()}\") }\n";
        let broken = "fn compute() -> int { 1\nfn main() { print(\"{compute()}\") }\n"; // missing `}`

        let mut session = bone_session(files(v1)).expect("session v1");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        assert!(session.diagnostics().is_empty(), "v1 is clean");

        // Broken edit: total parse -> Ok, but a diagnostic appears. The watcher's
        // gate (`!errs.is_empty()`) fires here, so it serves last good + no reload.
        session.update_file("main.rock", broken).expect("broken update is total (Ok)");
        assert!(
            !session.diagnostics().is_empty(),
            "a broken edit must surface a diagnostic so the watcher skips the reload",
        );

        // Recovery to a NEW valid program: clean, and a full rebuild matches a
        // fresh session built from v2 (no corruption from the broken intermediate).
        session.update_file("main.rock", v2).expect("recover to v2");
        assert!(session.diagnostics().is_empty(), "v2 is clean again");
        let recovered = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit recovered").0;

        let fresh_session = bone_session(files(v2)).expect("fresh v2 session");
        let fresh_mid = skull::user_module_id(fresh_session.workspace()).unwrap();
        let fresh = skull::mir_emit::emit_full(fresh_session.workspace(), fresh_mid).expect("emit fresh").0;

        assert_eq!(
            recovered.lua, fresh.lua,
            "recovered build after a broken intermediate must equal a fresh build",
        );
    }

    /// Regression for the "stale main.lua / `IVec2_ct nil`" desync class
    /// (TODO.md): a body edit's overlay references a value-struct ctor (e.g.
    /// `P_ct`) emitted GLOBAL into main.lua by `value_struct_defs`. The frozen
    /// baseline (v0) must already carry EVERY defined value struct's ctor -- even
    /// `P`, which v0's bodies never construct -- so when the edited body in the
    /// overlay first constructs `P`, its `P_ct` resolves against the baseline
    /// rather than crashing on nil. Pins the two invariants that close the bug:
    /// `value_struct_defs` emits ALL value structs (not use-gated), and the frozen
    /// baseline is a real full build. If either regresses, the composed run errors
    /// (nil ctor) while fresh succeeds, and `assert_runtime_eq` fails loudly.
    #[test]
    fn overlay_referencing_baseline_value_ctor_runtime_eq() {
        // `P` is DEFINED in both versions but only CONSTRUCTED in v1's `compute`.
        // v1 uses `P` as a WHOLE value (`==`), not pure field reads: SROA
        // (`scalarize_struct_cells`) scalarizes a field-read-only struct away
        // (no ctor), which would defeat this test's point -- it must reference the
        // baseline's `P_ct`, so it must keep the cdata (`==` forces that).
        let decl = "value P { x: int, y: int }\n";
        let v0 = format!("{decl}fn compute() -> int {{ 1 }}\nfn main() {{ print(\"{{compute()}}\") }}\n");
        let v1 = format!(
            "{decl}fn compute() -> int {{ let p = P {{ x: 5, y: 6 }}\n let q = P {{ x: 2, y: 9 }}\n if p == q {{ 0 }} else {{ p.x + p.y }} }}\nfn main() {{ print(\"{{compute()}}\") }}\n"
        );

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;
        // Sanity: the frozen baseline defines P's ctor even though nothing builds P.
        assert!(stale_main.contains("P_ct"), "baseline must emit every value-struct ctor, even unused ones");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "constructing an already-defined value struct is a body edit, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        assert!(overlay.lua.contains("P_ct"), "overlay's edited body must reference P_ct");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "overlay refs baseline value ctor (P 5+6=11)");
    }

    /// One concrete edit, fully spelled out, so the mechanism is legible before
    /// the fuzz generalizes it. v0: f0=1, f1 inlines f0, main prints both -> "1\n1".
    /// Edit f0 to 2. The overlay must re-emit f0 AND the inlined caller f1 AND
    /// main (which inlines them) -> "2\n2". If any of those is missing from the
    /// overlay, the stale baseline's 1 leaks through.
    #[test]
    fn concrete_inlined_edit_runtime_eq() {
        let v0 = "@inline\nfn f0() -> int { 1 }\n\
                  fn f1() -> int { f0() }\n\
                  fn main() { print(\"{f0()}\") print(\"{f1()}\") }\n";
        let v1 = "@inline\nfn f0() -> int { 2 }\n\
                  fn f1() -> int { f0() }\n\
                  fn main() { print(\"{f0()}\") print(\"{f1()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "literal edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline alone prints 1,1 -- so a leak is observable.
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "1\n1", "baseline should print the STALE values");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "concrete f0 1->2");
    }

    /// A `const`'s value edit hot-reloads through the overlay. v0: FOO=5, main
    /// prints it -> "5". Edit FOO to 10: the overlay must re-emit FOO's init
    /// UNGUARDED so the reload reassigns it -> "10". If the overlay omits the const
    /// init, the stale baseline's 5 leaks through and diverges from the fresh build.
    #[test]
    fn concrete_const_value_edit_runtime_eq() {
        let v0 = "const FOO: int = 5\nfn main() { print(\"{FOO}\") }\n";
        let v1 = "const FOO: int = 10\nfn main() { print(\"{FOO}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "const value edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline alone prints the OLD const, so a leak is observable.
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "5", "baseline prints the STALE const");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "const FOO 5->10");
    }

    /// A `static` local's INITIALIZER edit hot-reloads through the overlay -- the
    /// static analog of `concrete_const_value_edit_runtime_eq` and the direct
    /// regression for `docs/static-hot-reload-overlay-bug.md`. Unlike a const, a
    /// static is lazy (inits on first reach) and name-keyed (its cell persists
    /// across reloads BY DESIGN), so the overlay must emit an UNGUARDED reset (nil
    /// the cell + clear its done-flag) for a static whose init key changed, or the
    /// redefined fn re-reads the stale warmed cell and the edit is silently ignored.
    ///
    /// Models the LIVE game: the baseline runs once (warming the static to 5), THEN
    /// the overlay loads and it runs again. Without the reset the second call still
    /// prints 5; with it, 10. (`assert_runtime_eq`'s compose order never warms the
    /// static before the overlay, so it can't see this -- hence the bespoke run.)
    #[test]
    fn concrete_static_init_edit_runtime_eq() {
        let v0 = "fn probe() -> int { static x = 5\n x }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { static x = 10\n x }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "static initializer edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // Warm the static in the frozen baseline (first main() -> inits x=5, sets
        // the done-flag), THEN load the overlay + call again. The bug is observable
        // precisely because the cell is already warm: without the overlay's reset
        // the redefined `probe` skips re-init and re-reads the stale 5.
        let composed = run_parts(&[&stale_main, SYS_STUB, "main()", &overlay.lua, "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "5\n10",
                "warm baseline prints 5; after the overlay reload the static re-inits to 10",
            ),
            ExecOutcome::Errored => panic!("composed baseline+overlay run errored"),
        }
    }

    /// A `static` inside a NESTED `fn` must hot-reload too. Nested fns are hoisted
    /// to sibling chunks, so their statics have to be folded into the enclosing
    /// chunk's `statics` (both the full-reload preamble and the overlay read
    /// `chunk.statics`); if they aren't, the cell is invisible to the reset logic
    /// and an initializer edit is silently ignored on every reload.
    #[test]
    fn nested_fn_static_init_edit_runtime_eq() {
        let v0 = "fn probe() -> int { fn inner() -> int { static x = 5\n x }\n inner() }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { fn inner() -> int { static x = 10\n x }\n inner() }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "nested static init edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        let composed = run_parts(&[&stale_main, SYS_STUB, "main()", &overlay.lua, "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "5\n10",
                "nested-fn static must re-init on initializer edit through the overlay",
            ),
            ExecOutcome::Errored => panic!("composed baseline+overlay run errored"),
        }
    }

    /// Accumulation across TWO overlays (the analog of the const "edit A then B"
    /// oracle, point 2 in this module's doc). A reset line fires only on the reload
    /// that dofiles the overlay carrying it, so it must appear ONLY in the overlay
    /// of the reload where the init key changed -- NOT in later overlays, or an
    /// unrelated follow-up edit would re-reset and wipe accumulated static state.
    /// Requires advancing the per-cell baseline as overlays apply; a baseline only
    /// rebuilt on full builds would re-emit the reset here and lose the accumulation.
    #[test]
    fn static_init_edit_then_unrelated_edit_preserves_reinit_overlay() {
        // `probe` accumulates into a static counter each call; DO_RESET (unused
        // here) keeps the fn shape stable. v1 edits the initializer (0 -> 100); v2
        // is an UNRELATED edit (dead `let`), initializer still 100.
        let v0 = "fn probe() -> int { static n = 0\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";
        let v1 = "fn probe() -> int { static n = 100\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";
        let v2 = "fn probe() -> int { static n = 100\n let unused = 0\n n = n + 1\n n }\nfn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let r1 = session.update_file("main.rock", v1).expect("update v1");
        let overlay1 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r1.reinferred);
        let r2 = session.update_file("main.rock", v2).expect("update v2");
        let overlay2 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r2.reinferred);

        // Models the live game: main.lua(v0) loaded once; each overlay dofile'd in
        // turn; probe reached (accumulating) between reloads. n: inits 0 -> 1,2 ->
        // reload1 re-inits to 100 -> 101,102 -> reload2 UNRELATED must PRESERVE ->
        // 103,104. A re-reset at reload2 would drop back to 101.
        let composed = run_parts(&[
            &stale_main, SYS_STUB,
            "main()", "main()",            // 1, 2   (init 0)
            &overlay1.lua,
            "main()", "main()",            // 101, 102  (re-init 100)
            &overlay2.lua,
            "main()", "main()",            // 103, 104  (preserve)
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n2\n101\n102\n103\n104",
                "reload1 re-inits to 100; reload2 (unrelated) must preserve, not re-reset",
            ),
            ExecOutcome::Errored => panic!("composed baseline+overlays run errored"),
        }
    }

    /// The §A narrowing on the overlay path: an UNRELATED edit in a fn that owns a
    /// static must NOT reset the static -- the overlay reconsiders the fn's cells
    /// (it's in `changed_since_full`) but the key is unchanged, so the warm value
    /// is preserved. Mirror of `static_hot_reload.rs::static_preserved_when_unrelated_body_edited`
    /// but through the overlay. Without the key check (reset every changed fn's
    /// statics) this would wrongly re-init the static and diverge.
    #[test]
    fn static_preserved_under_unrelated_overlay_edit() {
        // `DO_SET` distinguishes PRESERVE (n still 99) from RESET (n re-inits 42).
        let v0 = "global DO_SET: bool = false\n\
                  fn probe() -> int { static n = 42\n if DO_SET { n = 99 }\n n }\n\
                  fn main() { print(\"{probe()}\") }\n";
        // Unrelated body edit (dead `let`), initializer `42` untouched.
        let v1 = "global DO_SET: bool = false\n\
                  fn probe() -> int { static n = 42\n let unused = 0\n if DO_SET { n = 99 }\n n }\n\
                  fn main() { print(\"{probe()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "unrelated body edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // Warm the static to 99 (DO_SET=true, one call), load the overlay, then call
        // again with DO_SET=false: an unrelated edit must PRESERVE n -> still 99.
        let composed = run_parts(&[&stale_main, SYS_STUB, "DO_SET = true", "main()", &overlay.lua, "DO_SET = false", "main()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "99\n99",
                "unrelated edit must not reset the static: warmed 99 survives the reload",
            ),
            ExecOutcome::Errored => panic!("composed baseline+overlay run errored"),
        }
    }

    /// const-on-const: editing an UPSTREAM const must update a DOWNSTREAM const
    /// that reads it, in dependency order -- even with reversed source order (B
    /// declared before A). v0: B = A + 1, A = 5 -> B = 6. Edit A to 10: the overlay
    /// must re-emit BOTH consts with A's init BEFORE B's, so B becomes 11. If the
    /// overlay re-emits only the changed const (A) or emits in source order, B
    /// stays stale (6) and diverges from the fresh build (11).
    #[test]
    fn const_reading_const_overlay_runtime_eq() {
        let v0 = "const B: int = A + 1\nconst A: int = 5\nfn main() { print(\"{B}\") }\n";
        let v1 = "const B: int = A + 1\nconst A: int = 10\nfn main() { print(\"{B}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let stale_main = full0.lua;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "const value edit should be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Sanity: the stale baseline prints the OLD downstream value (B = old A + 1).
        let baseline_only = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        if let ExecOutcome::Output(o) = baseline_only {
            assert_eq!(normalize_stdout(&o).trim(), "6", "baseline prints stale B = old A + 1");
        } else {
            panic!("stale baseline failed to run");
        }

        assert_runtime_eq(&stale_main, &overlay.lua, &fresh.lua, "const B reads A, A 5->10 => B 11");
    }

    /// Narrowing: editing one const re-emits ONLY the affected consts -- the
    /// changed one plus its transitive dependents (consts that read it) -- not
    /// every const. `A` is edited; `DEP = A + 1` must follow; the independent
    /// `UNRELATED` must NOT appear in the overlay at all.
    #[test]
    fn const_overlay_reemits_only_affected() {
        let v0 = "const A: int = 5\nconst DEP: int = A + 1\nconst UNRELATED: int = 99\nfn main() { print(\"{A} {DEP} {UNRELATED}\") }\n";
        let v1 = "const A: int = 10\nconst DEP: int = A + 1\nconst UNRELATED: int = 99\nfn main() { print(\"{A} {DEP} {UNRELATED}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // A changed -> its ginit fn + line re-emitted. DEP reads A -> its line
        // re-emitted (ginit unchanged, lives in main.lua). UNRELATED is independent
        // -> it must NOT appear in the overlay at all.
        assert!(overlay.lua.contains("__skull_ginit_A"), "changed const A must be re-emitted:\n{}", overlay.lua);
        assert!(overlay.lua.contains("DEP = "), "dependent const DEP (reads A) must be re-emitted:\n{}", overlay.lua);
        assert!(!overlay.lua.contains("UNRELATED"), "independent const UNRELATED must NOT be re-emitted:\n{}", overlay.lua);

        // ...and the result is still runtime-correct.
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        assert_runtime_eq(&full0.lua, &overlay.lua, &fresh.lua, "narrowed const overlay: A 5->10, DEP follows, UNRELATED untouched");
    }

    /// Positive control: the oracle must not be vacuously green. With the overlay
    /// OMITTED, the frozen baseline serves stale values and MUST diverge from a
    /// fresh build -- proving `assert_runtime_eq`'s comparison has teeth (an
    /// overlay that silently dropped a function would look exactly like this).
    #[test]
    fn harness_detects_a_missing_overlay() {
        let v0 = "fn f0() -> int { 1 }\nfn main() { print(\"{f0()}\") }\n";
        let v1 = "fn f0() -> int { 9 }\nfn main() { print(\"{f0()}\") }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let stale_main = skull::mir_emit::emit_full(session.workspace(), mid).expect("full v0").0.lua;
        session.update_file("main.rock", v1).expect("update v1");
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("full v1").0.lua;

        // Empty overlay (just the stub) == "the overlay dropped everything".
        let stale = run_parts(&[&stale_main, SYS_STUB, "main()"]);
        let fresh_run = run_parts(&[&fresh, SYS_STUB, "main()"]);
        match (stale, fresh_run) {
            (ExecOutcome::Output(s), ExecOutcome::Output(f)) => assert_ne!(
                normalize_stdout(&s),
                normalize_stdout(&f),
                "missing overlay should leave a STALE/fresh divergence the oracle can detect"
            ),
            _ => panic!("both versions should run cleanly"),
        }
    }

    // --- runtime accumulation fuzz: frozen v0 baseline + cumulative overlay -----
    //
    // The byte-identical fuzz (`incremental_watch_tests::fuzz_incremental_matches_fresh`)
    // proves `emit_incremental` (full reassembly) == fresh. This one RUNS the
    // OVERLAY: a single frozen `main.lua` (v0) plus an overlay that accumulates
    // EVERY change from v0 through vk -- never a full rebuild in between, the
    // worst case for the accumulation rule. After each edit, `frozen ++ overlay`
    // executed under luajit must print exactly what a fresh `emit_full(vk)` prints.
    // Editing an inlined callee forces its callers (incl. `main`, which inlines
    // them) into the overlay; a dropped accumulation or a missed caller re-emit
    // surfaces as a wrong line of output.

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    #[derive(Clone)]
    enum Body {
        Const(i64),
        Call(usize), // f{j}, j < i: a DAG inline edge (no recursion)
    }

    /// Render the f-graph plus a stable `main` that prints every fN on its own
    /// line. `main` is never mutated; it inlines the leaf fns, so it lands in the
    /// overlay whenever any callee it reaches changes.
    fn render(bodies: &[Body]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        // `@inline` so the graph actually inlines under the default policy -- the
        // overlay accumulation fuzz must exercise inlined-callee re-emit at runtime.
        for (i, b) in bodies.iter().enumerate() {
            match b {
                Body::Const(k) => { let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ {k} }}"); }
                Body::Call(j) => { let _ = writeln!(s, "@inline\nfn f{i}() -> int {{ f{j}() }}"); }
            }
        }
        let _ = write!(s, "fn main() {{");
        for i in 0..bodies.len() {
            let _ = write!(s, " print(\"{{f{i}()}}\")");
        }
        let _ = writeln!(s, " }}");
        s
    }

    fn mutate(bodies: &mut [Body], rng: &mut Rng) -> usize {
        let i = rng.pick(bodies.len());
        bodies[i] = if i == 0 || rng.pick(2) == 0 {
            Body::Const(rng.pick(1000) as i64)
        } else {
            Body::Call(rng.pick(i))
        };
        i
    }

    fn run_overlay_fuzz(seed: u64, n_fns: usize, steps: usize) {
        let mut rng = Rng(seed.max(1));
        let mut bodies: Vec<Body> = (0..n_fns)
            .map(|i| {
                if i > 0 && rng.pick(2) == 0 { Body::Call(rng.pick(i)) } else { Body::Const(rng.pick(1000) as i64) }
            })
            .collect();

        let v0 = render(&bodies);
        let mut session = bone_session(files(&v0)).unwrap_or_else(|e| panic!("seed {seed}: session init: {}", e.message));
        let mid = skull::user_module_id(session.workspace()).unwrap();
        // The FROZEN baseline: emitted once at v0, never rebuilt. The overlay must
        // carry the cumulative v0->vk diff on top of it.
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid)
            .unwrap_or_else(|e| panic!("seed {seed}: emit_full v0: {}", e.message));
        let stale_main = full0.lua;

        for step in 0..steps {
            let changed = mutate(&mut bodies, &mut rng);
            let text = render(&bodies);
            let result = session
                .update_file("main.rock", &text)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: update_file: {}", e.message));

            let diags = session.diagnostics();
            assert!(
                diags.is_empty(),
                "seed {seed} step {step} (edited f{changed}): unexpected diagnostics: {:?}\n{text}",
                diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
            );

            let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
            let fresh = skull::mir_emit::emit_full(session.workspace(), mid)
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: emit_full vk: {}", e.message))
                .0;

            assert_runtime_eq(
                &stale_main,
                &overlay.lua,
                &fresh.lua,
                &format!("seed {seed} step {step} (edited f{changed}, cumulative since v0)"),
            );
        }
    }

    #[test]
    fn runtime_accumulation_fuzz() {
        // Each luajit spawn costs ~10-30ms, so this runs fewer/shorter than the
        // byte fuzz. Override with SKULL_RT_FUZZ_SEEDS / SKULL_RT_FUZZ_STEPS for a
        // longer soak.
        let steps: usize = std::env::var("SKULL_RT_FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        let seeds: u64 = std::env::var("SKULL_RT_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
        for seed in 1..=seeds {
            run_overlay_fuzz(seed.wrapping_mul(2654435761), 8, steps);
        }
    }

    // --- real-corpus composition: overlay every fn, RUN @tests vs full ---------
    //
    // The synthetic fuzz covers later-wins + accumulation on an int-only graph.
    // Real corpus files exercise code SHAPES the synthetic graph can't reach
    // (struct/value-struct ctors, generics, closures, coroutines, maps, vecs).
    // Here v0 == vN: emit_full -> baseline, then route EVERY user function through
    // the overlay (the maximal body edit) and assert each `@test` prints the same
    // composed (baseline ++ overlay) as it does from the baseline alone. Identity
    // bodies mean any divergence is a COMPOSITION bug -- redefining a real-shaped
    // function after main.lua, or the `clear_all_systems` rebind, breaking it.

    use bone_semantic::hir::BodyOwner;

    /// Resolve a workspace-root-relative path. A unit test's CWD is the crate
    /// dir (`crates/rock-cli/`), but the corpus lives at the workspace root, so
    /// anchor to `CARGO_MANIFEST_DIR`'s grandparent.
    fn ws_path(rel: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(rel)
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Default)]
    struct FileResult {
        compared: usize,
        divergences: Vec<(String, String, String)>,
        /// Skipped because the overlay's rebind re-registers engine systems -- the
        /// corpus tier has no real `clear_all_systems`/registries, so the rebind
        /// double-registers. These belong to the SIM tier (real engine).
        skipped_system: bool,
    }

    /// Compare overlay-composed vs full-build execution for one file. A file that
    /// fails to parse/typecheck/emit, or whose tests error on BOTH sides,
    /// contributes 0 comparisons rather than a divergence -- this oracle compares
    /// skull-vs-skull (overlay vs full), so a file skull can't compile is out of
    /// scope.
    fn overlay_check_file(file: &str) -> FileResult {
        let mut r = FileResult::default();
        let Ok(src) = fs_err::read_to_string(file) else { return r };
        let Ok(user) = skull_parser::parse_module(&src, ustr::Ustr::from(file)) else { return r };
        let user_file = user.file;
        let Ok(ws) = crate::bone_backend::check_with_stdlib([user]) else { return r };
        let mid = crate::bone_backend::entry_module_id(&ws, user_file);
        let Ok((full, mut cache)) = skull::mir_emit::emit_full(&ws, mid) else { return r };
        let tests = skull::module_tests(&ws, mid);
        if tests.is_empty() {
            return r;
        }

        // The maximal body edit: re-route every user function through the overlay.
        let reinferred: Vec<(bone_semantic::prelude::ModuleId, BodyOwner)> = ws
            .module(mid)
            .functions
            .iter()
            .map(|(fid, _)| (mid, BodyOwner::Function(fid)))
            .collect();
        let overlay = skull::mir_emit::emit_overlay(&ws, &mut cache, &reinferred);

        // The overlay rebind calls `__rock_rebind_system` (an engine fn absent in
        // a standalone @test compile) for each changed decorated fn. Files with
        // decorated systems are the SIM tier's job -- skip them here rather than
        // error on the undefined helper.
        if overlay.lua.contains("__rock_rebind_system(") {
            r.skipped_system = true;
            return r;
        }

        let stub = stub_for(&full.lua);
        for t in &tests {
            let call = format!("{}()", t.name);
            let composed = run_parts(&[&full.lua, stub, &overlay.lua, &call]);
            let fresh = run_parts(&[&full.lua, stub, &call]);
            match (composed, fresh) {
                (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                    r.compared += 1;
                    let (nc, nf) = (normalize_stdout(&c), normalize_stdout(&f));
                    // Map `pairs()` order legitimately varies; same line-set in a
                    // different order is not a divergence.
                    if nc != nf && !super::is_order_only(&nc, &nf) {
                        r.divergences.push((file.to_string(), t.name.clone(), super::first_diff_line(&nf, &nc)));
                    }
                }
                // Both error: out of scope (skull may not run this test standalone).
                (ExecOutcome::Errored, ExecOutcome::Errored) => {}
                (ExecOutcome::Output(_), ExecOutcome::Errored) => {
                    r.compared += 1;
                    r.divergences.push((file.to_string(), t.name.clone(), "composed runs, full errors".into()));
                }
                (ExecOutcome::Errored, ExecOutcome::Output(_)) => {
                    r.compared += 1;
                    r.divergences.push((file.to_string(), t.name.clone(), "overlay BREAKS a test the full build runs".into()));
                }
            }
        }
        r
    }

    struct SweepTotals {
        files_with_tests: usize,
        compared: usize,
        skipped_system: usize,
        divergences: Vec<(String, String, String)>,
    }

    /// Run `overlay_check_file` over a list, catching per-file bone panics (some
    /// inputs panic in inference). Returns totals.
    fn sweep(files: &[String]) -> SweepTotals {
        use rayon::prelude::*;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let per: Vec<FileResult> = files
            .par_iter()
            .map(|f| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| overlay_check_file(f)))
                    .unwrap_or_default()
            })
            .collect();
        std::panic::set_hook(prev);
        SweepTotals {
            compared: per.iter().map(|r| r.compared).sum(),
            files_with_tests: per.iter().filter(|r| r.compared > 0).count(),
            skipped_system: per.iter().filter(|r| r.skipped_system).count(),
            divergences: per.into_iter().flat_map(|r| r.divergences).collect(),
        }
    }

    /// Fast always-on subset: a curated spread of real code shapes. Asserts no
    /// divergence AND that a meaningful number of tests actually ran (guards
    /// against the suite silently skipping everything if emit/parse breaks).
    #[test]
    fn overlay_composition_curated_real_files() {
        let curated = [
            "tests/adversarial_generics.rock",
            "tests/adversarial_map.rock",
            "tests/adversarial_strings.rock",
            "tests/adversarial_overloads.rock",
            "tests/adversarial_newtypes.rock",
            "tests/adversarial_iterators.rock",
            "tests/adversarial_functions_lambdas.rock",
            "tests/adversarial_coroutines.rock",
            "tests/adversarial_nullable_refinement.rock",
            "tests/adversarial_native_arrays.rock",
        ];
        let present: Vec<String> = curated.iter().map(|s| ws_path(s)).filter(|f| std::path::Path::new(f).exists()).collect();
        assert!(present.len() >= 8, "curated corpus files missing: only {} present", present.len());
        let t = sweep(&present);
        assert!(
            t.divergences.is_empty(),
            "overlay composition diverged from full build on real files:\n{}",
            t.divergences.iter().map(|(f, ts, d)| format!("  {f}::{ts}: {d}")).collect::<Vec<_>>().join("\n")
        );
        assert!(
            t.files_with_tests >= 6 && t.compared >= 15,
            "too few real comparisons (files_with_tests={}, tests={}) -- emit/parse likely broke",
            t.files_with_tests, t.compared
        );
    }

    // --- sim tier: the overlay rebind re-points the system registry ------------
    //
    // The corpus tier can't reach this: a `@update`/`@draw` decorator registers a
    // function VALUE into a registry (`UpdateSystem { func, name, stage }`), so
    // later-wins on the global does NOT update the copy already in the registry.
    // The overlay's rebind (`clear_all_systems()` + re-apply every decorator) is
    // what re-points it. This needs a running system loop + real registries --
    // here a SELF-CONTAINED fixture with the genuine engine `deco`/registry/clear
    // shapes (copied from `stone/engine/systems/systems.rock`), driven through the
    // REAL `emit_overlay`, executed under luajit. (`docs/skull-incremental-emit-split.md`.)

    /// A minimal game-shaped module: one `@update` system whose body writes
    /// `tick_val` to an observable global, plus the engine's deco/registry/clear
    /// machinery and a frame runner. `run_frame()` drives the registry exactly as
    /// the engine's `run_update_systems` does (iterate, call `system.func()`).
    fn sim_fixture(tick_val: i32) -> String {
        format!(
            "struct UpdateSystem {{ func: fn() -> (), name: string, stage: int }}\n\
             global UPDATE_SYSTEMS: [UpdateSystem] = []\n\
             deco init update(stage: int = 100) {{\n\
                UPDATE_SYSTEMS.push(UpdateSystem {{ func: transmute(target.func), name: transmute(target.name), stage: stage }})\n\
             }}\n\
             @nomangle\n\
             inline(lua) fn __rock_rebind_system(name: string, f: any) = \"\"\"\n\
                for i = 1, UPDATE_SYSTEMS.len do if UPDATE_SYSTEMS._data[i].name == name then UPDATE_SYSTEMS._data[i].func = f end end\n\
                return nil\n\
             \"\"\"\n\
             global TICK_OUT: int = 0\n\
             @update\n\
             fn tick() {{ TICK_OUT = {tick_val} }}\n\
             fn run_frame() {{ for s in UPDATE_SYSTEMS {{ s.func() }} }}\n"
        )
    }

    /// Run the registry-driving frame and report the observable global.
    const SIM_DRIVER: &str = "run_frame() print(TICK_OUT)";

    /// The real overlay's rebind must re-point the registry at the EDITED system
    /// body. Editing a decorated `@update` body, delivered as an overlay, must
    /// make the frame loop run the NEW body (== a fresh full build), not the stale
    /// function value the registry captured when `main.lua` registered it at load.
    ///
    /// Regression guard for the full chain: bone reports the decorated body in
    /// `reinferred` (`lower.rs` footprint fix), skull re-emits it into the overlay,
    /// and the rebind (`clear_all_systems()` + `__deco_update`) re-registers the
    /// fresh `tick`. `sim_without_rebind_runs_stale_body` is the negative control
    /// proving the rebind (not just later-wins) is what makes this work.
    #[test]
    fn sim_overlay_rebind_runs_new_system_body() {
        let v0 = sim_fixture(1);
        let v1 = sim_fixture(2);

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        // Baseline: the registry runs the v0 body.
        match run_parts(&[&full0.lua, SIM_DRIVER]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1", "baseline frame should run the v0 system body"),
            ExecOutcome::Errored => panic!("baseline fixture failed to run"),
        }

        let result = session.update_file("main.rock", &v1).expect("update v1");
        // The decorated body edit must re-infer the body (else the overlay can't
        // re-emit it). Guards the bone `lower.rs` footprint fix.
        assert_eq!(
            result.reinferred.len(), 1,
            "a decorated-fn body edit must re-infer exactly the edited body"
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        // The overlay carries the redefined body AND a targeted rebind of just the
        // changed decorated fn (in-place, not clear-all + re-register-all).
        assert!(
            overlay.lua.contains("function tick()")
                && overlay.lua.contains("__rock_rebind_system(\"tick\""),
            "overlay must redefine the body AND rebind the changed system:\n{}",
            overlay.lua
        );
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        let composed = run_parts(&[&full0.lua, &overlay.lua, SIM_DRIVER]);
        let fresh_run = run_parts(&[&fresh.lua, SIM_DRIVER]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "2", "fresh full build should run the v1 body");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "2",
                    "overlay rebind must run the EDITED decorated body, not the stale registry func"
                );
            }
            _ => panic!("composed or fresh run errored"),
        }
    }

    /// Positive control: WITHOUT the rebind -- a naive global redefinition (what
    /// later-wins alone gives) -- the registry keeps its stale func value and the
    /// frame loop runs the OLD body. Proves the rebind is load-bearing and a
    /// regression that dropped it would be caught by the test above.
    #[test]
    fn sim_without_rebind_runs_stale_body() {
        let v0 = sim_fixture(1);
        let session = bone_session(files(&v0)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let full = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full").0;

        // Redefine `tick` globally but do NOT clear+re-register (no rebind).
        let naive_redef = "function tick() TICK_OUT = 2 end";
        match run_parts(&[&full.lua, naive_redef, SIM_DRIVER]) {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1",
                "without the rebind the registry must run the STALE body (this is the bug the rebind fixes)"
            ),
            ExecOutcome::Errored => panic!("fixture failed to run"),
        }
    }

    /// A `co fn` BODY edit, delivered through a REAL `Session.update_file`, must
    /// re-emit the coroutine into the overlay so a fresh invocation runs the new
    /// body -- exactly like an ordinary fn. The corpus sweep can't catch this: it
    /// fabricates `reinferred` from every function directly, bypassing the Session
    /// (where the fast-path body reparse slices the item from `fn`, dropping `co`).
    /// This drives the genuine edit pipeline end to end.
    #[test]
    fn coroutine_body_edit_overlay_runs_new_body() {
        let v0 = "co fn counter() -> Coroutine<int> {\n  yield 1\n  yield 2\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";
        let v1 = "co fn counter() -> Coroutine<int> {\n  yield 10\n  yield 20\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        match run_parts(&[&full0.lua, stub_for(&full0.lua), "main()"]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1 2", "baseline co fn body"),
            ExecOutcome::Errored => panic!("baseline coroutine fixture failed to run"),
        }

        let result = session.update_file("main.rock", v1).expect("update v1");
        // The co fn body edit must re-infer exactly the edited body.
        assert_eq!(result.reinferred.len(), 1, "a co fn body edit re-infers exactly one body");

        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        // The overlay must carry the re-emitted coroutine (its top-level step fn).
        assert!(
            overlay.lua.contains("counter__step("),
            "overlay must re-emit the coroutine's step fn:\n{}",
            overlay.lua
        );

        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "main()"]);
        let fresh_run = run_parts(&[&fresh.lua, stub_for(&fresh.lua), "main()"]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "10 20", "fresh full build runs the v1 co fn body");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "10 20",
                    "overlay must run the EDITED coroutine body, not the stale one"
                );
            }
            _ => panic!("composed or fresh coroutine run errored"),
        }
    }

    /// A FLAT-ENUM-yielding `co fn` body edit, through the overlay, must re-emit the
    /// frame-slot yield channel (the `__step`'s `s.__y*` slot writes + the ctor's
    /// `__advance`/`__resume` fields on the merged frame) and a freshly-created coroutine must run the new body,
    /// threading the yield through the slots -- no `{__variant}` box on the concrete
    /// consumer path. Guards the frame-slot channel (`docs/coroutine-allocation-plan.md`
    /// Part A) against a hot-reload regression; the scalar-yield test above does not
    /// exercise the channel (int yields keep the multi-return path).
    #[test]
    fn flat_enum_coroutine_body_edit_overlay_threads_and_runs_new_body() {
        // The `Act(() -> ())` closure payload makes Ev NON-cdata-eligible, so it is a
        // genuinely FLAT (boxed) enum that rides the frame-slot channel -- an
        // all-scalar-payload enum would scalarize as a `_ct` cdata instead (mirrors
        // `tests/enum_thread_yield.rock`).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn main() {{\n  let c = gen()\n  let e0 = c.resume()\n  let e1 = c.resume()\n  let s0 = if e0 != null {{ score(e0) }} else {{ -1 }}\n  let s1 = if e1 != null {{ score(e1) }} else {{ -1 }}\n  print(\"{{s0}} {{s1}}\")\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        match run_parts(&[&full0.lua, stub_for(&full0.lua), "main()"]) {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1 3", "baseline flat-enum co fn (Tick(1)=1, Pair(1,2)=3)"),
            ExecOutcome::Errored => panic!("baseline flat-enum coroutine fixture failed to run"),
        }

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        // The overlay must re-emit the whole frame-slot channel.
        assert!(overlay.lua.contains("gen__step("), "overlay must re-emit the step fn:\n{}", overlay.lua);
        assert!(overlay.lua.contains(".__y0 = "), "overlay step must write yield leaves to frame slots:\n{}", overlay.lua);
        assert!(
            overlay.lua.contains("function gen__advance(")
                && overlay.lua.contains("__advance = gen__advance")
                && overlay.lua.contains("__resume = gen__resume"),
            "overlay must re-emit the per-co-fn __advance/__resume fns + the ctor storing them as fields:\n{}",
            overlay.lua
        );

        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "main()"]);
        let fresh_run = run_parts(&[&fresh.lua, stub_for(&fresh.lua), "main()"]);
        match (composed, fresh_run) {
            (ExecOutcome::Output(c), ExecOutcome::Output(f)) => {
                assert_eq!(normalize_stdout(&f).trim(), "10 30", "fresh v1: Tick(10)=10, Pair(10,20)=30");
                assert_eq!(
                    normalize_stdout(&c).trim(),
                    "10 30",
                    "overlay must run the EDITED flat-enum body via the re-emitted frame-slot channel"
                );
            }
            _ => panic!("composed or fresh flat-enum coroutine run errored"),
        }
    }

    /// The load-bearing hot-reload case: a flat-enum coroutine created + first-resumed
    /// under v0, then resumed again ACROSS a body-edit overlay on the SAME live object
    /// (the engine's per-frame poll pattern). The live object keeps its v0
    /// `__advance`/`__resume` closures, but `__advance` calls `gen__step` by GLOBAL
    /// name, so a mid-flight resume runs the NEW step -- writing the new yield leaves
    /// into the SAME reused `__y*` slots on the object/frame (the enum shape is
    /// unchanged), which the (baseline, unchanged) `pump` reads directly. If
    /// `__advance` had captured the step by value, resume #2 would run the stale v0
    /// body (Pair(1,2)=3, not Pair(10,20)=30).
    #[test]
    fn live_flat_enum_coroutine_survives_overlay_and_threads_new_body() {
        // `Act(() -> ())` forces Ev to be a FLAT (boxed) enum on the frame-slot
        // channel (an all-scalar-payload enum would be a `_ct` cdata instead).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // start() + pump() under v0 (Tick(1)=1), then overlay, then pump() x3 on the
        // SAME live coroutine under v1: Pair(10,20)=30, Done=100, then done -> -1.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            &overlay.lua,
            "pump()",
            "pump()",
            "pump()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n30\n100\n-1",
                "live coroutine: v0 first yield (Tick=1), then v1 body across the reload (Pair=30, Done=100, done)"
            ),
            ExecOutcome::Errored => panic!("live flat-enum coroutine reload fixture failed to run"),
        }
    }

    /// Layout drift, cell shift: a live coroutine suspended across a BodyOnly
    /// overlay that INSERTS a frame-resident local before an existing one,
    /// shifting its dense slot. The new step would read the shifted local at a
    /// slot the old frame never wrote -- the frame layout stamp kills the
    /// coroutine cleanly instead. The inserted `z` is a GENUINE frame cell
    /// (read after the first yield, so it must survive suspension); a dead local
    /// -- `let z = 999` never read -- is now a ghost dropped from the frame and
    /// does NOT shift the layout (the P2-finding fix; that path has its own
    /// structural test in skull/tests/frame_slot_naming.rs).
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_cell_layout_shift_overlay_stamp_kills() {
        // `Act(() -> ())` forces Ev flat (frame-slot channel); `mk()` keeps the
        // locals as real calls so copy-prop/DCE can't erase the frame cells.
        // v1 inserts `z` AND reads it after yield 1 (`a = a + z`), so `z` is a
        // real frame cell that shifts `b`'s slot -- a genuine layout change.
        let v0 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  fn start() { GEN = gen() }\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";
        let v1 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let z = mk(999)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  a = a + z\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  fn start() { GEN = gen() }\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "inserting a let is a body edit -- must be BodyOnly (the premise), got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // start() + pump() under v0 (Tick(7)=7, suspended at state 1), overlay,
        // then pump() again on the SAME live coroutine. The frame layout stamp
        // rejects the v0 frame in the v1 step (the genuine `z` frame cell shifted
        // `b`'s slot): the check errors INSIDE the pcall, the coroutine dies
        // cleanly (state -2, on_killed, last_error explains) and pump reads null
        // -> -1. Pre-stamp this CRASHED the consumer (arithmetic on the nil read
        // from the shifted slot).
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\n-1\n-1\n-1",
                "a cell-shifting overlay must stamp-kill the live coroutine, not corrupt it"
            ),
            ExecOutcome::Errored => panic!("cell-shift + live resume crashed -- the layout stamp regressed"),
        }
    }

    /// Layout drift, state renumber: yields are numbered by order, so a BodyOnly
    /// overlay that ADDS a yield point renumbers the state machine and a live
    /// coroutine's `__state` would dispatch to the wrong resume point (re-run or
    /// skip code) -- the stamp's per-state variant tags catch it and kill the
    /// coroutine cleanly. Cells are deliberately untouched (const payloads only)
    /// so this pins the STATE component of the stamp specifically.
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_yield_renumber_overlay_stamp_kills() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n{extra}  yield Ev::Tick(1)\n  print(\"fire\")\n  yield Ev::Tick(2)\n  print(\"cleanup\")\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  yield Ev::Done\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a yield is a body edit -- must be BodyOnly (the premise), got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // v0: pump() -> Tick(1)=1, suspended at state 1 (before print("fire")).
        // Overlay renumbers: the new Ev::Done yield becomes state 1, so the live
        // frame's __state=1 would resume at the WRONG yield point (pre-stamp it
        // replayed the already-delivered Tick(1), then ran fire/cleanup a pump
        // late). The per-state variant tags in the layout stamp catch the
        // renumbering: resume #2 stamp-kills the coroutine -> -1, no replay.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n-1\n-1\n-1",
                "a yield-renumbering overlay must stamp-kill the live coroutine, not teleport its resume point"
            ),
            ExecOutcome::Errored => panic!("yield-renumber + live resume errored (expected the clean kill output)"),
        }
    }

    /// `yield*` under hot reload: a live outer+inner delegation chain must pick up
    /// a BodyOnly edit to the INNER co fn's body. The chain is advance-by-field
    /// (outer holds the inner object) + step-by-global-name (inner's stale advance
    /// resolves `inner__step` at call time), and both mechanisms must compose
    /// across the delegation boundary. Guards the Part B kill/advance/delegate
    /// refactor (docs/coroutine-allocation-plan.md).
    #[test]
    fn live_yield_star_delegation_survives_inner_body_overlay() {
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn inner() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Tick({b})\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n  yield* inner()\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = outer() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "a constant tweak in the inner co fn must be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // pump #1 under v0: outer primes inner, re-yields inner's Tick(1). Overlay
        // (layout-stable: constants only). pump #2 resumes the live chain: the
        // inner's stale advance runs the NEW inner__step -> Tick(20). pump #3:
        // inner done -> outer's delegate exits, outer completes -> -1.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n20\n-1",
                "a live yield* chain must thread a BodyOnly inner edit (v0 Tick(1), then v1 Tick(20), then done)"
            ),
            ExecOutcome::Errored => panic!("live yield* delegation reload fixture failed to run"),
        }
    }

    /// A stamp-kill (or ANY error) of an outer coroutine suspended mid-`yield*`
    /// must chain into the delegated inner: explicit `kill()` recurses through
    /// `__inner` (firing the inner's on_killed), and the error path must match --
    /// otherwise the inner is orphaned and its cleanup hook silently never runs
    /// (found live: mag mob soft-locked in attack state after a reload).
    #[test]
    fn stamp_killed_outer_chains_kill_into_delegated_inner() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 global INNER_KILLED: int = 0\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 fn note_inner_killed() {{ INNER_KILLED = INNER_KILLED + 1 }}\n\
                 co fn inner() -> Coroutine<Ev> {{\n  on_killed(() => note_inner_killed())\n  yield Ev::Tick(1)\n  yield Ev::Tick(2)\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n{extra}  yield* inner()\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = outer() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n\
                 fn report() {{ print(\"{{INNER_KILLED}}\") }}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  let z = mk(9)\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "outer's added local must be BodyOnly, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // pump #1: outer primes inner -> Tick(1). Overlay shifts OUTER's layout.
        // pump #2: outer stamp-killed -> -1, and the inner's on_killed must fire.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "report()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1\n-1\n1",
                "a stamp-killed outer must chain-kill its delegated inner (inner's on_killed fires exactly once)"
            ),
            ExecOutcome::Errored => panic!("stamp-kill inner-chain fixture failed to run"),
        }
    }

    /// The mob-soft-lock scenario in miniature: a coroutine holding a game
    /// invariant cleans up via `defer`, then a layout-shifting overlay
    /// stamp-kills it mid-flight. The defer must run -- and it must be the
    /// creation-time (v0) defer against the v0 frame: `__defers` stores the
    /// per-site fns BY VALUE precisely so stale frames never run current-layout
    /// cleanup code against old-layout cells.
    #[test]
    fn stamp_kill_runs_creation_time_defers() {
        // v1 inserts `z` and reads it after the first yield (`a = a + z`), so `z`
        // is a genuine frame cell shifting `b`'s slot -- a real layout change.
        // (A dead `let z = 999` is now a ghost that does NOT shift the frame.)
        let prog = |extra: &str, cross: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 global BUSY: int = 0\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  BUSY = 1\n  defer {{ BUSY = 0 }}\n  let a = mk(7)\n{extra}  let b = mk(35)\n  yield Ev::Tick(a)\n{cross}  yield Ev::Tick(b)\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n\
                 fn report() {{ print(\"busy:{{BUSY}}\") }}\n"
            )
        };
        let (v0, v1) = (prog("", ""), prog("  let z = mk(999)\n", "  a = a + z\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly));
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);

        // pump #1: Tick(7), BUSY=1, suspended. Overlay (cell shift). pump #2:
        // stamp-kill -> -1, and the v0 defer must have cleared BUSY.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "report()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\n-1\nbusy:0",
                "a stamp-killed coroutine must run its creation-time defers (invariant released, no soft-lock)"
            ),
            ExecOutcome::Errored => panic!("stamp-kill defer fixture failed to run"),
        }
    }

    /// A coroutine hook (`on_killed`/`on_resume`/`on_hot_reload`) argument must
    /// be a lambda literal: the hook is peeled to the constructor, which can
    /// only re-render a lambda with the frame context. Any other fn-typed
    /// expression used to SILENTLY install no hook (docs/unfuck-backlog.md) --
    /// it must be a compile error instead.
    #[test]
    fn non_lambda_coroutine_hook_arg_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   fn cleanup() { print(\"x\") }\n\
                   co fn gen() -> Coroutine<Ev> {\n  on_killed(cleanup)\n  yield Ev::Tick(1)\n}\n\
                   fn main() { let c = gen() }\n";
        let session = bone_session(files(src)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        match skull::mir_emit::emit_full(session.workspace(), mid) {
            Err(e) => assert!(
                format!("{e:?}").contains("lambda"),
                "rejection must say the hook needs a lambda, got: {e:?}"
            ),
            Ok((full, _)) => {
                // Per-fn skips are also an acceptable surface -- but the co fn
                // must NOT emit with a silently-dropped hook.
                assert!(
                    !full.lua.contains("function gen__step("),
                    "a co fn with a non-lambda hook arg must not emit:\n{}",
                    full.lua
                );
            }
        }
    }

    /// `yield` inside a `defer` body is a compile error (a defer body cannot
    /// suspend -- it may run during terminal cleanup with no resume protocol).
    #[test]
    fn yield_inside_defer_body_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   co fn gen() -> Coroutine<Ev> {\n  defer { yield Ev::Tick(1) }\n  yield Ev::Tick(2)\n}\n\
                   fn main() { let c = gen() }\n";
        let session = bone_session(files(src)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let err = skull::mir_emit::emit_full(session.workspace(), mid);
        match err {
            Err(e) => assert!(
                format!("{e:?}").contains("defer"),
                "rejection must mention defer, got: {e:?}"
            ),
            Ok((full, _)) => {
                // Emission collects per-fn skips rather than failing the whole
                // unit -- accept either surface, but the co fn must NOT emit.
                assert!(
                    !full.lua.contains("function gen__step("),
                    "a co fn with `yield` inside a defer body must not emit:\n{}",
                    full.lua
                );
            }
        }
    }

    /// Layout drift via the Interface tier (which does NOT protect live
    /// coroutines): a full rebuild is a re-`dofile`, not a restart -- mutable
    /// globals carry the `_G.HOT_RELOAD` preservation guard, so a live coroutine
    /// held in a global SURVIVES the full reload, and its stale `__advance`
    /// resolves the step by global name into the NEW program. A realistic
    /// Interface edit (widen a variant + use the new payload, which also inserts
    /// a local) would then drive the old frame with a step compiled for a
    /// different layout -- the stamp kills the survivor cleanly instead.
    /// docs/coroutine-allocation-plan.md "Layout drift across reloads".
    #[test]
    fn live_coroutine_survives_full_reload_and_shape_edit_stamp_kills() {
        let v0 = "enum Ev { Tick(int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  fn start() { GEN = gen() }\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";
        // Widen Tick BEYOND the current max payload width (Pair's 2 -> Tick's 3),
        // so the frame-slot yield channel genuinely grows -- the shape edit shifts
        // the layout stamp (a within-width widen would be absorbed by the union
        // channel and, post-P2-fix, is correctly layout-stable). The enum edit is
        // Interface tier, delivered via a FULL rebuild.
        let v1 = "enum Ev { Tick(int, int, int), Pair(int, int), Done, Act(() -> ()) }\n\
                  global GEN: Coroutine<Ev>? = null\n\
                  fn mk(n: int) -> int { return n }\n\
                  co fn gen() -> Coroutine<Ev> {\n  let a = mk(7)\n  let b = mk(35)\n  yield Ev::Tick(a, b, a)\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}\n\
                  fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x, y, z) => x + y + z\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n\
                  fn start() { GEN = gen() }\n\
                  fn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, _cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "an enum shape edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Premise (non-inverted, stays green): the live coroutine SURVIVES the full
        // re-dofile -- the mutable-global preservation guard keeps GEN.
        let survival = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.GEN ~= nil)",
        ]);
        match survival {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\ntrue",
                "a live coroutine in a mutable global must survive a full re-dofile (HOT_RELOAD guard)"
            ),
            ExecOutcome::Errored => panic!("full-reload survival fixture failed to run"),
        }

        // Resuming the survivor: the v1 step's layout stamp (yield channel
        // widened by the Tick payload growing past the union width) rejects the
        // v0 frame -- the coroutine dies cleanly through the pcall path and pump
        // prints -1. Pre-stamp this CRASHED the consumer on the shifted frame
        // read, through the very tier the hot-reload contract once claimed was safe.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "start()",
            "pump()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.GEN ~= nil)",
            "pump()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\ntrue\n-1",
                "a shape-edit full reload must stamp-kill the surviving live coroutine, not corrupt it"
            ),
            ExecOutcome::Errored => panic!("full-reload survivor resume crashed -- the layout stamp regressed"),
        }
    }

    /// PREMISE pin for the schema-drift restart policy
    /// (docs/hot-reload-schema-drift-plan.md): a struct SHAPE edit is an
    /// Interface-tier full rebuild, but the full reload is a re-`dofile` that
    /// preserves any mutable global whose declared-type STRING is unchanged --
    /// and `"Mob?"` names the shape without describing it. A live instance
    /// therefore SURVIVES the reload with its old field set while every reader
    /// recompiles to the new one: the recompiled reader nil-reads the added
    /// field and dies on the first arithmetic touch (or silently propagates
    /// nil through dynamic sinks). Non-inverted -- this documents the emit
    /// layer's real behavior; the restart policy prevents the scenario one
    /// layer up (the stone watcher), which this harness deliberately bypasses.
    #[test]
    fn struct_field_add_full_reload_nil_poisons_live_instances() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp}\") }\n}\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp + m.armor}\") }\n}\n\
                  fn main() { }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let full0 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0").0;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "a struct shape edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Survival premise: the live v0 instance rides through the full
        // re-dofile untouched -- `"Mob?"` didn't change, so the guard skips
        // the re-init and the old-shape table persists.
        let survival = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "print(_G.M ~= nil and _G.M.armor == nil)",
        ]);
        match survival {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "10\ntrue",
                "a live struct instance must survive the full re-dofile WITHOUT the added field"
            ),
            ExecOutcome::Errored => panic!("struct-shape survival fixture failed to run"),
        }

        // Corruption: the v1 reader assumes `armor: int` on an instance that
        // never had it -- `m.hp + m.armor` is arithmetic on nil.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "describe()",
        ]);
        match composed {
            ExecOutcome::Errored => {} // the pinned corruption
            ExecOutcome::Output(o) => panic!(
                "expected the recompiled reader to die on the nil-valued added field, got output:\n{}",
                normalize_stdout(&o)
            ),
        }
    }

    /// PREMISE pin for the schema-drift restart policy, value-struct class
    /// (docs/hot-reload-schema-drift-plan.md): LuaJIT `ffi.cdef` typedef
    /// redefinition is a SILENT NO-OP -- the v1 chunk's cdef "succeeds" but
    /// `ffi.typeof` keeps the ORIGINAL layout for the life of the process.
    /// Value-struct construction is POSITIONAL in declared-field order
    /// (`P_ct(a, b)`), so after a field REORDER the v1 ctor call feeds v1-order
    /// args into the frozen v0 ctype: every field silently gets the wrong
    /// value. Not even a fresh post-reload construction is safe -- this is the
    /// class where no in-process response exists, the core restart argument.
    #[test]
    fn value_struct_reorder_full_reload_misassigns_positional_ctor() {
        let v0 = "value P { x: int, y: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";
        let v1 = "value P { y: int, x: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let full0 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0").0;

        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "a value-struct layout edit must be a full rebuild (the premise), got {:?}",
            result.tier
        );
        let full1 = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // v0 alone: sanity -- set/show agree on the layout.
        let fresh = run_parts(&[&full0.lua, stub_for(&full0.lua), "store_p()", "show()"]);
        match fresh {
            ExecOutcome::Output(o) => assert_eq!(normalize_stdout(&o).trim(), "1,2"),
            ExecOutcome::Errored => panic!("value-struct v0 fixture failed to run"),
        }

        // Composed: the v1 chunk re-cdefs P (silent no-op), then a FRESH
        // post-reload `set()` constructs `P { x: 1, y: 2 }` through the v1
        // positional order (y first) against the frozen v0 ctype (x first):
        // x=2, y=1. The pinned output is the SWAP -- silently wrong values,
        // no error anywhere.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "store_p()",
            "show()",
            "_G.HOT_RELOAD = true",
            &full1.lua,
            "store_p()",
            "show()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "1,2\n2,1",
                "expected the frozen-ctype positional misassignment (the pinned corruption)"
            ),
            ExecOutcome::Errored => panic!(
                "value-struct reorder fixture errored -- if ffi.cdef redefinition \
                 started FAILING loudly, the premise (and the plan doc) needs updating"
            ),
        }
    }

    /// A flat enum's DEFINITION change (here: a payload widening that adds a frame
    /// slot) must classify as an Interface edit -> full `main.lua` rebuild (+ overlay
    /// wipe), NOT a BodyOnly overlay. This is what keeps the frame-slot channel safe
    /// across a shape change: a live coroutine can only survive a BodyOnly reload, and
    /// its (v0) resume box-rebuild + the consumer's `__y*` reads assume the v0 slot
    /// count. Were a shape change a mere overlay, the new step would write a different
    /// number of slots against stale readers. The Interface tier forces a full rebuild
    /// (fresh program, no live objects carried over), so that mismatch is impossible.
    #[test]
    fn flat_enum_shape_edit_forces_full_rebuild_not_overlay() {
        let v0 = "enum Ev { Tick(int), Act(() -> ()) }\n\
                  co fn gen() -> Coroutine<Ev> { yield Ev::Tick(1) }\n\
                  fn main() { let c = gen() }\n";
        // Widen Tick to two payloads: Ev's flat shape grows a positional slot.
        let v1 = "enum Ev { Tick(int, int), Act(() -> ()) }\n\
                  co fn gen() -> Coroutine<Ev> { yield Ev::Tick(1, 2) }\n\
                  fn main() { let c = gen() }\n";

        let mut session = bone_session(files(v0)).expect("session v0");
        let result = session.update_file("main.rock", v1).expect("update v1");
        assert!(
            matches!(
                result.tier,
                bone_incremental::session::UpdateTier::Interface
                    | bone_incremental::session::UpdateTier::FullUser
            ),
            "an enum shape change must force a full rebuild (not a BodyOnly overlay), got {:?}",
            result.tier
        );
    }

    /// `@on_reload` callbacks (e.g. the engine's `fire_live_coroutine_hot_reloads`,
    /// which drives every live coroutine's `on_hot_reload` hook) must fire on EVERY
    /// incremental reload, not just the first. `__rock_fire_hot_reload_callbacks()`
    /// snapshots-and-CLEARS `__rock_on_hot_reload`; on a FULL reload the
    /// `@on_reload` deco-init re-runs during `dofile(main.lua)` and repopulates it,
    /// but a body-edit OVERLAY only re-runs the CHANGED fns' chunks -- it never
    /// re-runs the (unchanged) `@on_reload` deco-init, so after the first fire the
    /// list is permanently empty. Symptom: editing a coroutine's `on_hot_reload {}`
    /// body prints once, then never again (mag `mob_attack_co`).
    ///
    /// Model: a `@on_reload` fn bumping an observable counter + an UNRELATED edited
    /// fn, two successive overlays each followed by a real
    /// `__rock_fire_hot_reload_callbacks()` (== two incremental reloads). The
    /// counter must reach 2.
    #[test]
    fn on_reload_callback_refires_across_successive_overlays() {
        let prog = |n: i32| {
            format!(
                "global FIRE_COUNT: int = 0\n\
                 @nomangle\n\
                 @on_reload\n\
                 fn bump_fire_count() {{ FIRE_COUNT = FIRE_COUNT + 1 }}\n\
                 fn unrelated() -> int {{ return {n} }}\n\
                 fn report() {{ print(\"{{FIRE_COUNT}}\") }}\n"
            )
        };
        let (v0, v1, v2) = (prog(1), prog(2), prog(3));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let r1 = session.update_file("main.rock", &v1).expect("update v1");
        let overlay1 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r1.reinferred);
        let r2 = session.update_file("main.rock", &v2).expect("update v2");
        let overlay2 = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r2.reinferred);

        // full0 registers bump_fire_count via @on_reload. Each overlay is a body
        // edit of `unrelated` (bump_fire_count is NEVER in the changed set), then a
        // real fire. Both fires must run the callback -> FIRE_COUNT == 2.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            &overlay1.lua,
            "__rock_fire_hot_reload_callbacks()",
            &overlay2.lua,
            "__rock_fire_hot_reload_callbacks()",
            "report()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "2",
                "the @on_reload callback must fire on BOTH incremental reloads"
            ),
            ExecOutcome::Errored => panic!("on_reload overlay fixture failed to run"),
        }
    }

    /// Editing the BODY of an `@on_reload` fn, delivered as an overlay, must run
    /// the NEW body on the next fire -- the name-lookup trick. `@on_reload`
    /// registers the fn's NAME (not its value), and `__rock_fire_hot_reload_callbacks`
    /// resolves `_G[name]` at fire time, so a redefined global propagates to the
    /// already-registered entry (same mechanic as a coroutine's `__step`). Requires
    /// `@nomangle` so the Lua global == the source name. Pre-fix the deco captured
    /// the function VALUE, so a body edit ran the stale original.
    #[test]
    fn on_reload_body_edit_runs_new_body_via_name_lookup() {
        let prog = |v: i32| {
            format!(
                "global OUT: int = 0\n\
                 @nomangle\n\
                 @on_reload\n\
                 fn refresh() {{ OUT = {v} }}\n\
                 fn report() {{ print(\"{{OUT}}\") }}\n"
            )
        };
        let (v0, v1) = (prog(1), prog(2));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let r1 = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &r1.reinferred);

        // full0 registers `refresh` by name. The overlay redefines the `refresh`
        // global (new body); the fire must resolve and run the NEW body -> OUT == 2.
        // `refresh` is decorated, so the overlay emits a `__rock_rebind_system`
        // rebind (engine-provided; a no-op for the hot-reload list) -- stub it.
        let composed = run_parts(&[
            &full0.lua,
            stub_for(&full0.lua),
            "function __rock_rebind_system(name, f) end",
            &overlay.lua,
            "__rock_fire_hot_reload_callbacks()",
            "report()",
        ]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "2",
                "fire must run the EDITED @on_reload body (name-lookup), not the stale original"
            ),
            ExecOutcome::Errored => panic!("on_reload body-edit fixture failed to run"),
        }
    }

    /// `@on_reload` registers by NAME and re-resolves `_G[name]` at fire time, so
    /// the Lua global must equal the source name -- i.e. the fn must be `@nomangle`
    /// (else a same-named overload could mangle it and the lookup would silently
    /// miss). skull rejects a `@on_reload` fn that isn't `@nomangle` at emit time.
    #[test]
    fn on_reload_without_nomangle_is_a_compile_error() {
        let prog = "@on_reload\nfn refresh() {}\nfn main() {}\n";
        let session = bone_session(files(prog)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let res = skull::mir_emit::emit_full(session.workspace(), mid);
        let err = res.err().expect("@on_reload without @nomangle must be a compile error");
        assert!(
            err.message.contains("nomangle"),
            "error should name the @nomangle requirement, got: {}",
            err.message
        );
    }

    /// The positive control: `@nomangle @on_reload` emits cleanly.
    #[test]
    fn on_reload_with_nomangle_emits_ok() {
        let prog = "@nomangle\n@on_reload\nfn refresh() {}\nfn main() {}\n";
        let session = bone_session(files(prog)).expect("session");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        assert!(
            skull::mir_emit::emit_full(session.workspace(), mid).is_ok(),
            "@nomangle @on_reload must emit cleanly"
        );
    }

    /// Whole-corpus sweep, gated (heavy: thousands of luajit spawns). Run with
    /// `SKULL_OVERLAY_CORPUS=1 cargo nextest run -p rock-cli overlay_composition_full_corpus`.
    #[test]
    fn overlay_composition_full_corpus() {
        if std::env::var("SKULL_OVERLAY_CORPUS").is_err() {
            return; // opt-in; the curated subset is the always-on gate
        }
        let root = std::env::var("SKULL_OVERLAY_ROOT").unwrap_or_else(|_| ws_path("tests"));
        let files = super::collect_rock_files(&root).expect("collect");
        let files: Vec<String> = files.into_iter().filter(|f| !f.contains("/benches/")).collect();
        let t = sweep(&files);
        eprintln!(
            "overlay corpus sweep: {} files, {} with tests, {} tests compared, {} skipped (system/sim-tier), {} divergences",
            files.len(), t.files_with_tests, t.compared, t.skipped_system, t.divergences.len()
        );
        assert!(
            t.divergences.is_empty(),
            "overlay composition diverged on {} test(s):\n{}",
            t.divergences.len(),
            t.divergences.iter().map(|(f, ts, d)| format!("  {f}::{ts}: {d}")).collect::<Vec<_>>().join("\n")
        );
    }

    // -- frame-lambda hoist (docs/coroutine-frame-lambda-plan.md P2) ----------
    // These pin the reload/layout contract of P1's co-fn lambda hoist: a
    // capturing co-fn lambda is an env record whose `__fn` names a hoisted
    // top-level body emitted IN THE SAME CHUNK as the step (const-hoist
    // precedent), so a BodyOnly overlay carries it. A test that would also pass
    // against pre-P1 emission (inline `function(...)` closures, no
    // `gen__step__lambda<k>` global) is not evidence -- each asserts the
    // hoisted body is present in the overlay.

    /// The frame layout stamp for a coroutine ctor `function <ctor>()`: the
    /// `__layout = <N>` literal the ctor writes. Two builds whose only diff is a
    /// lambda-local must produce the SAME stamp (P1: lambda locals left the
    /// layout canon); a co-fn-level local shifts it.
    fn layout_stamp(lua: &str, ctor: &str) -> i64 {
        let marker = format!("function {ctor}()");
        let start = lua.find(&marker).unwrap_or_else(|| panic!("no ctor `{ctor}` in emitted lua"));
        let tail = &lua[start..];
        let key = "__layout = ";
        let lpos = tail.find(key).expect("ctor has no __layout stamp");
        let after = &tail[lpos + key.len()..];
        let end = after.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(after.len());
        after[..end].parse().expect("parse __layout stamp")
    }

    /// A capturing co-fn lambda whose BODY is edited (operator `+` -> `*`): the
    /// overlay must carry the hoisted `gen__step__lambda0` body, and a
    /// coroutine created AFTER the reload runs the new lambda behavior. Equals a
    /// fresh full build of v1. The `overlay.lua.contains("gen__step__lambda0")`
    /// assertion is what discriminates P1 from pre-P1 (which had no hoisted
    /// global -- the lambda was an inline closure inside the step).
    #[test]
    fn cofn_capturing_lambda_body_edit_overlay_created_after_reload() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 3\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(4))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "editing a lambda operator is a body edit, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        assert!(
            overlay.lua.contains("gen__step__lambda0"),
            "the overlay must carry the hoisted lambda body (const-hoist precedent); \
             absence means pre-P1 inline-closure emission"
        );
        // Created AFTER the overlay -> the v1 lambda (4*3=12), then done -> -1.
        // "12\n-1" IS a fresh v1 full build's output, so this doubles as the
        // fresh-equivalence check (the program has no `main`, so the generic
        // `assert_runtime_eq` driver doesn't apply -- start/pump is the driver).
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), &overlay.lua, "start()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "12\n-1",
                "a coroutine created after the reload runs the new hoisted lambda body (4*3=12)"
            ),
            ExecOutcome::Errored => panic!("post-reload coroutine with hoisted lambda failed to run"),
        }
    }

    /// The snapshot-at-creation contract (Risks / watch-outs: "Record staleness
    /// across hot reload is the same contract as the old inline closure"). A
    /// LIVE coroutine created BEFORE the reload survives a layout-stable body
    /// edit. A record created before the reload holds a STALE `__fn` value (the
    /// global was read into the table at creation); a record created after
    /// resume-post-reload reads the NEW global. This deliberately diverges from
    /// a fresh full build (which is uniformly v1), so it is a raw assertion.
    #[test]
    fn cofn_lambda_record_snapshot_stale_across_reload_new_record_fresh() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 5\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n  let g = (x: int) => x {op} base\n  yield Ev::Tick(g(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
        // Layout-stable premise: an operator edit doesn't touch the frame canon.
        assert_eq!(
            layout_stamp(&full0.lua, "gen"),
            {
                let mut s = bone_session(files(&v1)).expect("session v1 probe");
                let m = skull::user_module_id(s.workspace()).unwrap();
                let f = skull::mir_emit::emit_full(s.workspace(), m).expect("emit v1 probe").0;
                let _ = &mut s;
                layout_stamp(&f.lua, "gen")
            },
            "an operator-only edit must be layout-stable (no frame-cell change)"
        );

        let result = session.update_file("main.rock", &v1).expect("update v1");
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        assert!(overlay.lua.contains("gen__step__lambda1"), "overlay must carry the hoisted g body");

        // start() + pump() under v0: f = record snapshotting the OLD lambda0
        // (x+5), f(3)=8. Overlay (op *). pump #2 on the SAME live coroutine:
        // the stored record f is STALE (still x+5) -> 8. pump #3 creates a fresh
        // record g reading the NEW lambda1 (x*5) -> 15. pump #4 done -> -1.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "8\n8\n15\n-1",
                "a pre-reload record is a stale fn snapshot (8), a post-reload record is fresh (15)"
            ),
            ExecOutcome::Errored => panic!("live coroutine with stale lambda record failed to run"),
        }
    }

    /// A lambda-local-only edit is layout-stable: dense per-frame slot naming
    /// (docs/coroutine-frame-lambda-plan.md "P2 finding", now FIXED) keys both
    /// the emitted `s.c<slot>` field names AND the layout stamp on a dense
    /// ordinal that lambda-internal cell allocation cannot perturb, so a live
    /// coroutine SURVIVES the edit instead of stamp-killing. Was a sentinel
    /// (`assert_ne`, "7\n-1\n-1") pinning the two raw-id-perturbation causes it
    /// fixes:
    ///   (A) the canon hashed frame cells by RAW cell-arena id; lowering
    ///       allocates a lambda's internal cells BEFORE the enclosing `let`'s
    ///       cell, so adding `let y` inside the lambda shifted the raw id of
    ///       every frame cell after it (record `f`: c3 -> c4). Slots absorb it.
    ///   (B) an UNUSED lambda-local fell back to ROOT ownership and became a
    ///       frame cell outright. The ghost-cell drop (referenced-nowhere ->
    ///       out of the frame) removes it. (This test uses a USED local, so it
    ///       exercises (A); the ghost drop has its own structural test.)
    #[test]
    fn cofn_lambda_local_edit_shifts_layout_live_survives() {
        // v0 body: `x + base`; v1 body: `{ let y = base + base; x + y }` (used
        // local -- reason (A): raw-id shift, not the dead-cell fallback (B)).
        let prog = |body: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 4\n  let f = (x: int) => {body}\n  yield Ev::Tick(f(3))\n  let g = (x: int) => {body}\n  yield Ev::Tick(g(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let v0 = prog("x + base");
        let v1 = prog("{ let y = base + base; x + y }");

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a lambda-local is a body edit, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        // Dense slots leave the stamp fixed under a lambda-internal cell edit.
        assert_eq!(
            layout_stamp(&full0.lua, "gen"),
            layout_stamp(&fresh.lua, "gen"),
            "a lambda-local edit must be layout-stable (dense slots absorb the raw-id shift)"
        );

        // pump #1 under v0: f(3)=7. Overlay. pump #2 on the live coroutine: the
        // stamp still matches -> survives -> f is unchanged (x+base=7) so 3+4+4=11
        // via the new body path; pump #3 done -> -1.
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\n11\n-1",
                "a layout-stable lambda-local edit keeps the live coroutine running (7 then 11)"
            ),
            ExecOutcome::Errored => panic!("lambda-local edit scenario crashed instead of surviving"),
        }
    }

    /// The converse: adding a co-fn-LEVEL local DOES shift the frame layout, so
    /// a live coroutine across that overlay is stamp-killed cleanly. Pairs with
    /// the lambda-local test to pin that P1 moved ONLY lambda-owned cells out of
    /// the canon, not co-fn-level ones.
    #[test]
    fn cofn_toplevel_local_edit_shifts_layout_live_killed() {
        // `mk()` keeps the inserted co-fn local as a real call so DCE can't
        // erase its frame cell (same guard as the cell-shift stamp test).
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = mk(4)\n{extra}  let f = (x: int) => x + base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 fn start() {{ GEN = gen() }}\n\
                 fn pump() {{\n  let c = GEN\n  if c != null {{\n    let e = c.resume()\n    if e != null {{ print(\"{{score(e)}}\") }} else {{ print(\"-1\") }}\n  }}\n}}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  let z = mk(9)\n  yield Ev::Tick(z)\n"));

        let mut session = bone_session(files(&v0)).expect("session v0");
        let mid = skull::user_module_id(session.workspace()).unwrap();
        let (full0, mut cache) = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");

        let result = session.update_file("main.rock", &v1).expect("update v1");
        assert!(
            matches!(result.tier, bone_incremental::session::UpdateTier::BodyOnly),
            "adding a co-fn local is a body edit, got {:?}",
            result.tier
        );
        let overlay = skull::mir_emit::emit_overlay(session.workspace(), &mut cache, &result.reinferred);
        let fresh = skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v1").0;

        assert_ne!(
            layout_stamp(&full0.lua, "gen"),
            layout_stamp(&fresh.lua, "gen"),
            "a co-fn-level local must shift the frame layout stamp"
        );

        // pump #1 under v0: f(3)=7. Overlay (z shifts the frame). pump #2 on the
        // live coroutine: the stamp mismatch errors inside the pcall -> clean
        // kill -> -1; subsequent pumps stay -1 (dead).
        let composed = run_parts(&[&full0.lua, stub_for(&full0.lua), "start()", "pump()", &overlay.lua, "pump()", "pump()"]);
        match composed {
            ExecOutcome::Output(o) => assert_eq!(
                normalize_stdout(&o).trim(),
                "7\n-1\n-1",
                "a co-fn-level local shift must stamp-kill the live coroutine cleanly"
            ),
            ExecOutcome::Errored => panic!("co-fn local shift + live resume crashed instead of clean kill"),
        }
    }
}

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
    let mut session = crate::bone_backend::bone_session(files.clone())
        .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;
    let warm_ms = ms(t_warm);

    let (baseline_lua, mut cache, full_ms) = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let t_full = Instant::now();
        let (emit, c) = skull::mir_emit::emit_full(ws, mid)
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
        std::process::exit(1);
    }
}

/// `rock iter-incremental <project>` -- tight-loop sampling profiler for the REAL
/// stone incremental path. Warm a Session + `emit_full` once, then repeatedly
/// `update_file` + `emit_overlay` ONE body, alternating between two whitespace
/// states so every pass is a genuine change (not a content-hash no-op). Reports
/// the timing distribution (mean/p50/p99/min/max/stddev) + overlay size + the
/// per-phase breakdown of BOTH halves: `update_file` (clone/cook/lower/infer, from
/// bone's `phase_timings`) and `emit_overlay`. It calls the EXACT same
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
pub fn skull_iter_incremental(path: &str, iters: usize, warmup: usize) -> Result<()> {
    use std::path::PathBuf;

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

    let mut session = crate::bone_backend::bone_session(files.clone())
        .map_err(|e| anyhow::anyhow!("session warm failed: {}", e.message))?;

    let mut cache = {
        let ws = session.workspace();
        let mid = skull::user_module_id(ws)
            .map_err(no_user_module)?;
        let (_emit, c) = skull::mir_emit::emit_full(ws, mid)
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
    let (target, state_a, state_b) = if let Ok(fname) = std::env::var("SKULL_ITER_FN") {
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

    let iters = iters.max(1);
    let target_name = std::path::Path::new(&target)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let mut last_update = 0.0;
    let mut last_diag = 0.0;
    let mut last_emit = 0.0;
    let mut last_bytes = 0usize;
    let mut last_reinferred = 0usize;
    let mut last_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let mut last_update_phases: Vec<(&'static str, std::time::Duration)> = Vec::new();

    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &state_a } else { &state_b };

        let t_u = Instant::now();
        let upd = session
            .update_file(&target, text)
            .map_err(|e| anyhow::anyhow!("update_file: {}", e.message))?;
        let update_ms = ms(t_u);
        // bone's per-edit phase breakdown (clone/cook/lower/infer), set by
        // rebuild_body_incremental inside the SAME update_file the stone loop calls.
        let update_phases = session.workspace().phase_timings.clone();

        // `SkullStone::recompile` calls `diagnostics()` inside its `cook` timing --
        // an O(workspace) error scan + sort over every module. Measure it here too
        // so the iter-incremental "cook" matches the live one (the live cook ALSO
        // includes disk read of the changed file, which an in-memory loop can't).
        let t_d = Instant::now();
        let _ = session.diagnostics();
        let diag_ms = ms(t_d);

        // Profile the REAL stone path: `emit_overlay` (no reassembly), matching
        // what `SkullStone::recompile` writes on a body edit.
        let (emit_ms, overlay_bytes, phases) = {
            let ws = session.workspace();
            let t_e = Instant::now();
            let e = skull::mir_emit::emit_overlay(ws, &mut cache, &upd.reinferred);
            (ms(t_e), e.lua.len(), e.phases)
        };

        if i >= warmup {
            samples.push(update_ms + diag_ms + emit_ms);
            last_update = update_ms;
            last_diag = diag_ms;
            last_emit = emit_ms;
            last_bytes = overlay_bytes;
            last_reinferred = upd.reinferred.len();
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
        "  update+emit per pass [ms]:  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}  stddev {:.3} (cv {:.1}%)",
        mean, pct(50.0), pct(99.0), sorted[0], sorted[sorted.len() - 1], stddev, cv
    );
    eprintln!("\n  last-pass breakdown (the two-file split path):");
    eprintln!(
        "    Session update_file:            {last_update:.3} ms  (re-lower+re-infer ONLY the {last_reinferred} changed body/bodies)"
    );
    for (lbl, d) in &last_update_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    let update_known: f64 = last_update_phases.iter().map(|(_, d)| d.as_secs_f64() * 1000.0).sum();
    eprintln!("       (parse + classify, untimed: {:.3} ms)", (last_update - update_known).max(0.0));
    eprintln!("    diagnostics (O(workspace) scan): {last_diag:.3} ms  (live cook ALSO has disk read, not here)");
    eprintln!("    emit_overlay:                   {last_emit:.3} ms  ({last_bytes} B overlay)");
    for (lbl, d) in &last_phases {
        eprintln!("       {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stone --backend skull: two-file incremental output (main.lua + incremental.lua)
// ---------------------------------------------------------------------------

/// The game preamble injected at the top of `main.lua`: `GAME_SOURCE_DIR` (asset
/// resolution) + the embedded cimgui bindings. Factored so `skull_run_project`
/// and the stone two-file path build it identically.
fn skull_game_preamble(dir: &str) -> String {
    use std::path::PathBuf;
    let abs = PathBuf::from(dir).canonicalize().unwrap_or_else(|_| PathBuf::from(dir));
    let escaped = abs.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let mut full = format!("__ROCK_GAME_SOURCE_DIR = \"{escaped}\"\n");
    full.push_str(&rock_host::cimgui_embed::build_prelude());
    if !full.ends_with('\n') {
        full.push('\n');
    }
    full
}

/// Game-shaped `main.lua`: preamble + emit + `__rock_main_wrapper(main)` entry.
fn skull_game_main(preamble: &str, emit_lua: &str) -> String {
    let mut full = String::from(preamble);
    full.push_str(emit_lua);
    if emit_lua.contains("function main(") {
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
    main_path: &std::path::Path,
    incremental_path: &std::path::Path,
) -> Result<usize> {
    let main = skull_game_main(preamble, emit_lua);
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
    /// structural edit, or initial build).
    Full { bytes: usize, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// Body edit: only `incremental.lua` rewritten (the overlay). `reinferred` is
    /// the number of bodies this edit re-inferred (the edit's churn). `emit_ms` is
    /// pure `emit_overlay` (directly comparable to `iter-incremental`); `write_ms`
    /// is the separate disk write of the overlay.
    Incremental { bytes: usize, reinferred: usize, cook_ms: f64, emit_ms: f64, write_ms: f64 },
    /// A type's runtime SHAPE changed (docs/hot-reload-schema-drift-plan.md):
    /// live state cannot be migrated, so the game process must RESTART.
    /// Nothing was written -- the running process must never dofile the
    /// new-shape program; the restart path's `force_full` writes it after the
    /// kill. `types` names the drifted types for the why-log.
    ShapeChanged { types: Vec<String> },
    /// Nothing changed (content-hash no-op).
    NoChange,
}

/// Persistent stone-skull compile state: a warm bone `Session` + skull emit cache
/// driving the two-file split (`docs/skull-incremental-emit-split.md`). A body
/// edit re-cooks just the edited file and writes a small `incremental.lua` the
/// running game light-reloads; an interface/structural edit rewrites `main.lua`
/// (full) and wipes the overlay.
pub struct SkullStone {
    session: bone_incremental::session::Session,
    cache: skull::mir_emit::SkullEmitCache,
    preamble: String,
    dir: String,
    /// The discovered project sources (canonical id_path + contents), cached
    /// across edits. A recompile re-reads ONLY the changed files into this rather
    /// than re-reading the whole project (mag + engine deps) off disk every edit;
    /// the Session content-hashes unchanged files to a no-op anyway, so the disk
    /// read was the waste. A full re-discover only happens when the file SET
    /// changes (add/delete) or a changed path doesn't match a cached source.
    sources: SourceFiles,
    /// Per-type layout hashes of the last program the game process could have
    /// loaded. A full recompile whose hashes drifted returns
    /// `SkullReload::ShapeChanged` (restart request) instead of writing
    /// (docs/hot-reload-schema-drift-plan.md).
    shape_hashes: skull::ShapeHashes,
    /// Whether discovery keeps test-only files (tests/ dirs). The stone game
    /// build excludes them; the sim-runner build includes them (`@sim` bodies
    /// live there). Recorded so `recompile`'s full re-discover matches init.
    include_tests: bool,
}

/// The Session emit gate. `bone_session` lowering is TOTAL -- it always
/// produces HIR and reports type errors as collected diagnostics rather than
/// failing (unlike the compile path's `check_modules`, which runs
/// `body_errors` and returns `Err`). Every Session-based emit
/// (`init`/`recompile`/`force_full`) must call this before `emit_full`/
/// `emit_overlay`: otherwise emit drops an un-checkable body but keeps its
/// `__deco_init` registration, leaving the registered `func` a nil global -> a
/// runtime nil-call instead of the compile error. Renders each diagnostic and
/// bails; `ctx` distinguishes the call site in the bail message.
fn session_emit_verdict(
    session: &bone_incremental::session::Session,
    sources: &SourceFiles,
    ctx: &str,
) -> Result<()> {
    let errs = session.diagnostics();
    if !errs.is_empty() {
        for e in &errs {
            eprintln!("{}", render_project_err(sources, e));
        }
        anyhow::bail!("skull: {} type error(s) {ctx}", errs.len());
    }
    Ok(())
}

impl SkullStone {
    /// Initial build: warm the Session, `emit_full` -> game-shaped `main.lua`,
    /// wipe `incremental.lua`. Errors render against the project sources.
    /// Game build: excludes test-only files (tests/ dirs) so a broken sim/test
    /// can't break the running game. See `docs/sims-out-of-game-build-plan.md`.
    pub fn init(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, false)
    }

    /// Sim-runner build: same as [`SkullStone::init`] but discovery KEEPS
    /// test-only files -- `@sim` bodies live in `tests/sims/` and must be part
    /// of the emitted program.
    pub fn init_with_tests(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullStone> {
        Self::init_impl(dir, main_path, incremental_path, true)
    }

    fn init_impl(dir: &str, main_path: &std::path::Path, incremental_path: &std::path::Path, include_tests: bool) -> Result<SkullStone> {
        use std::path::PathBuf;
        let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
        let discovered = crate::unified_compile::discover_sources(&spec)?;
        let sources = if include_tests { discovered } else { discovered.production_only() };
        let files: Vec<(String, String)> = sources
            .files
            .iter()
            .map(|f| {
                let f = f.borrow();
                (f.file.to_id_path(), f.contents.clone())
            })
            .collect();

        let session = crate::bone_backend::bone_session(files)
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
        session_emit_verdict(&session, &sources, "in initial build")?;
        let (lua, cache) = {
            let ws = session.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::emit_full(ws, mid)
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
            // `_G.__ROCK_GLOBALS` for `--dump-state` (see `skull_emit_project`).
            let registry = build_rock_globals_registry(ws, &spec);
            (format!("{}{}", emit.lua, registry), c)
        };
        let preamble = skull_game_preamble(dir);
        write_full_main(&preamble, &lua, main_path, incremental_path)?;
        let shape_hashes = skull::type_shape_hashes(session.workspace());
        Ok(SkullStone { session, cache, preamble, dir: dir.to_string(), sources, shape_hashes, include_tests })
    }

    /// On a watch event: refresh the cached sources (re-read only `changed` off
    /// disk, or fully re-discover if the file SET changed), `update_file` every
    /// source (the Session content-hashes unchanged ones to a no-op), then route
    /// by the worst tier -- any interface/structural edit -> full `main.lua` +
    /// wipe overlay; otherwise the accumulated body edits -> `incremental.lua`
    /// overlay. `changed` is the watcher's changed-path set; pass empty to force a
    /// full re-discover.
    pub fn recompile(&mut self, changed: &[std::path::PathBuf], main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        use std::path::PathBuf;
        let t_cook = Instant::now();

        // Fast path: re-read only the changed files and feed JUST those to the
        // Session (the canonical incremental step -- the rest are already current
        // in the Session from init/prior edits, so re-feeding them is pure waste).
        // Fall back to a full re-discover + feed-all only when the file SET changes
        // (a new or deleted file) or a changed path doesn't canonicalize to a known
        // source -- so a path-mapping miss can never desync the module set, only
        // cost a (correct) full rebuild.
        let mut need_full = changed.is_empty();
        let mut feed: Vec<(String, String)> = Vec::new();
        if !need_full {
            for p in changed {
                let Ok(canon) = std::fs::canonicalize(p) else { need_full = true; break };
                let id = canon.to_string_lossy().into_owned();
                let Some(f) = self.sources.files.iter().find(|f| f.borrow().file.to_id_path() == id) else {
                    need_full = true; // new / unmatched file -> full re-discover
                    break;
                };
                let Ok(text) = fs_err::read_to_string(&canon) else { need_full = true; break };
                f.borrow_mut().contents = text.clone(); // keep the cache current
                feed.push((id, text));
            }
        }
        if need_full {
            let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(&self.dir));
            // Re-discover with the same test-file policy as init (game build
            // excludes tests/ dirs; sim build keeps them).
            let discovered = crate::unified_compile::discover_sources(&spec)?;
            self.sources = if self.include_tests { discovered } else { discovered.production_only() };
            feed = self.sources.files.iter()
                .map(|f| { let f = f.borrow(); (f.file.to_id_path(), f.contents.clone()) })
                .collect();
        }

        self.route_and_emit(&feed, main_path, incremental_path, t_cook)
    }

    /// In-memory edit: replace one source's contents WITHOUT touching disk and
    /// route/emit exactly like a watcher edit. The sim reload-injection path
    /// (`docs/sim-reload-harness-plan.md`): the runner edits a source in memory
    /// and writes only the overlay; the project tree is never mutated. Rejects
    /// a path the build didn't discover -- an edit to an unknown file would
    /// otherwise silently do nothing.
    pub fn apply_edit(&mut self, path: &std::path::Path, new_text: &str, main_path: &std::path::Path, incremental_path: &std::path::Path) -> Result<SkullReload> {
        let t_cook = Instant::now();
        let canon = std::fs::canonicalize(path)
            .map_err(|e| anyhow::anyhow!("apply_edit: cannot canonicalize `{}`: {e}", path.display()))?;
        let id = canon.to_string_lossy().into_owned();
        let Some(f) = self.sources.files.iter().find(|f| f.borrow().file.to_id_path() == id) else {
            anyhow::bail!("apply_edit: `{id}` is not a discovered source of `{}`", self.dir);
        };
        f.borrow_mut().contents = new_text.to_string(); // error rendering reads this
        let feed = vec![(id, new_text.to_string())];
        self.route_and_emit(&feed, main_path, incremental_path, t_cook)
    }

    /// The current in-session text of a source (reflects prior `apply_edit`s,
    /// which never touch disk). None if the path isn't a discovered source.
    pub fn source_text(&self, path: &std::path::Path) -> Option<String> {
        let id = std::fs::canonicalize(path).ok()?.to_string_lossy().into_owned();
        self.sources.files.iter()
            .find(|f| f.borrow().file.to_id_path() == id)
            .map(|f| f.borrow().contents.clone())
    }

    /// Feed source edits into the Session, route by the worst tier, and write
    /// the output: any interface/structural edit -> full `main.lua` + overlay
    /// wipe (behind the schema-drift gate); body-only edits -> `incremental.lua`
    /// overlay. Shared by the disk-driven watcher path ([`SkullStone::recompile`])
    /// and the in-memory sim reload injection ([`SkullStone::apply_edit`]).
    /// `t_cook` is the caller's front-end timer (includes its feed construction).
    fn route_and_emit(&mut self, feed: &[(String, String)], main_path: &std::path::Path, incremental_path: &std::path::Path, t_cook: Instant) -> Result<SkullReload> {
        use std::path::PathBuf;
        // Cheap Arc-clone of the cache handle so error rendering can read the
        // (shared) sources while the Session loop borrows `self.session` mutably.
        let sources = self.sources.clone();
        let mut reinferred: Vec<(bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner)> = Vec::new();
        let mut full = false;
        let mut any_change = false;
        for (p, text) in feed {
            let upd = self
                .session
                .update_file(p, text)
                .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
            match upd.tier {
                bone_incremental::session::UpdateTier::NoChange => {}
                bone_incremental::session::UpdateTier::BodyOnly => {
                    any_change = true;
                    reinferred.extend(upd.reinferred);
                }
                bone_incremental::session::UpdateTier::Interface | bone_incremental::session::UpdateTier::FullUser => {
                    any_change = true;
                    full = true;
                }
            }
        }

        session_emit_verdict(&self.session, &sources, "- serving last good output")?;
        if !any_change {
            return Ok(SkullReload::NoChange);
        }
        let cook_ms = t_cook.elapsed().as_secs_f64() * 1000.0;

        let t_emit = Instant::now();
        if full {
            // Schema-drift gate (docs/hot-reload-schema-drift-plan.md): live
            // data outlives every reload tier, so if any type's runtime layout
            // moved, hot-reloading the recompiled program would drive old-shape
            // state with new-shape readers. Request a process restart and defer
            // ALL writes -- the stone loop kills the process, then
            // `force_full` (canonicalize) writes this session's program for the
            // respawn. Re-baseline the stored hashes now: the delta is consumed
            // by this one restart.
            let new_shapes = skull::type_shape_hashes(self.session.workspace());
            let drifted = skull::shape_diff(&self.shape_hashes, &new_shapes);
            if !drifted.is_empty() {
                self.shape_hashes = new_shapes;
                return Ok(SkullReload::ShapeChanged { types: drifted });
            }
            let lua = {
                let ws = self.session.workspace();
                let mid = skull::user_module_id(ws)
                    .map_err(no_user_module)?;
                let (emit, c) = skull::mir_emit::emit_full(ws, mid)
                    .map_err(|e| anyhow::anyhow!("{}", render_project_err(&sources, &e)))?;
                self.cache = c; // resets changed_since_full
                // `_G.__ROCK_GLOBALS` for `--dump-state` (see `skull_emit_project`):
                // a structural edit can add/remove globals, so recompute on full.
                let spec =
                    crate::unified_compile::SourceSpec::Directory(PathBuf::from(&self.dir));
                let registry = build_rock_globals_registry(ws, &spec);
                format!("{}{}", emit.lua, registry)
            };
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            let bytes = write_full_main(&self.preamble, &lua, main_path, incremental_path)?;
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Full { bytes, cook_ms, emit_ms, write_ms })
        } else {
            let overlay = {
                let ws = self.session.workspace();
                skull::mir_emit::emit_overlay(ws, &mut self.cache, &reinferred)
            };
            let bytes = overlay.lua.len();
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            rock_host::project::write_atomic(incremental_path, &overlay.lua)?;
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
        session_emit_verdict(&self.session, &self.sources, "on restart fold")?;
        let lua = {
            let ws = self.session.workspace();
            let mid = skull::user_module_id(ws)
                .map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::emit_full(ws, mid)
                .map_err(|e| anyhow::anyhow!("skull force_full emit: {}", e.message))?;
            self.cache = c; // resets changed_since_full -- the overlay is now folded in
            // `_G.__ROCK_GLOBALS` for `--dump-state` (see `skull_emit_project`).
            let spec = crate::unified_compile::SourceSpec::Directory(
                std::path::PathBuf::from(&self.dir),
            );
            let registry = build_rock_globals_registry(ws, &spec);
            format!("{}{}", emit.lua, registry)
        };
        write_full_main(&self.preamble, &lua, main_path, incremental_path)?;
        // This program is what the (re)spawned process loads -- re-baseline the
        // shape hashes to it, including drift that arrived via a recompile that
        // bailed on a type error after mutating the session.
        self.shape_hashes = skull::type_shape_hashes(self.session.workspace());
        Ok(())
    }
}

#[cfg(test)]
mod shape_drift_tests {
    //! The schema-drift restart trigger (docs/hot-reload-schema-drift-plan.md):
    //! a type-SHAPE edit returns `ShapeChanged` (restart request) and DEFERS the
    //! `main.lua` write -- the running process must never hot-reload a program
    //! whose live data layout it doesn't have. Body and signature-only edits
    //! stay on the hot paths. The premise repros for the corruption itself are
    //! `struct_field_add_full_reload_nil_poisons_live_instances` and
    //! `value_struct_reorder_full_reload_misassigns_positional_ctor`.

    use super::{SkullReload, SkullStone};
    use std::path::PathBuf;

    fn boot(v0: &str) -> (tempfile::TempDir, SkullStone, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let main_lua = dir.path().join("main.lua");
        let inc_lua = dir.path().join("incremental.lua");
        let rock = dir.path().join("main.rock");
        std::fs::write(&rock, v0).unwrap();
        let stone = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init");
        (dir, stone, main_lua, inc_lua, rock)
    }

    #[test]
    fn struct_shape_edit_requests_restart_and_defers_the_write() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  fn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        let baseline = std::fs::read_to_string(&main_lua).unwrap();

        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("recompile v1");
        match &reload {
            SkullReload::ShapeChanged { types } => assert!(
                types.iter().any(|t| t == "Mob"),
                "the restart request must name the drifted type, got {types:?}"
            ),
            other => panic!("a struct shape edit must request a restart (ShapeChanged), got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            baseline,
            "the new-shape main.lua write must be DEFERRED -- the old process must never dofile it"
        );

        // The restart path canonicalizes: force_full writes the new program.
        stone.force_full(&main_lua, &inc_lua).expect("force_full");
        assert_ne!(
            std::fs::read_to_string(&main_lua).unwrap(),
            baseline,
            "canonicalize after the kill must write the deferred new-shape program"
        );

        // Re-baselined: recompiling the same source again is a no-op, not a
        // second restart request.
        let again = stone.recompile(&[rock], &main_lua, &inc_lua).expect("recompile same");
        assert!(
            matches!(again, SkullReload::NoChange),
            "the shape delta must be consumed by the restart, got {again:?}"
        );
    }

    #[test]
    fn value_struct_layout_edit_requests_restart() {
        let v0 = "value P { x: int, y: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n";
        let v1 = "value P { y: int, x: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);
        std::fs::write(&rock, v1).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("recompile v1");
        assert!(
            matches!(&reload, SkullReload::ShapeChanged { types } if types.iter().any(|t| t == "P")),
            "a value-struct field reorder freezes against the process ctype -- must restart, got {reload:?}"
        );
    }

    #[test]
    fn body_and_signature_edits_stay_on_the_hot_paths() {
        let v0 = "struct Mob { hp: int }\n\
                  fn heal(m: Mob) -> int { m.hp }\n\
                  fn main() { }\n";
        let body = "struct Mob { hp: int }\n\
                    fn heal(m: Mob) -> int { m.hp + 1 }\n\
                    fn main() { }\n";
        let sig = "struct Mob { hp: int }\n\
                   fn heal(m: Mob, bonus: int) -> int { m.hp + bonus }\n\
                   fn main() { }\n";
        let (_dir, mut stone, main_lua, inc_lua, rock) = boot(v0);

        std::fs::write(&rock, body).unwrap();
        let reload = stone.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("body edit");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a body edit must stay a hot overlay, got {reload:?}"
        );

        std::fs::write(&rock, sig).unwrap();
        let reload = stone.recompile(&[rock], &main_lua, &inc_lua).expect("sig edit");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a signature-only edit carries no data-shape drift -- full reload, NOT a restart, got {reload:?}"
        );
    }
}

#[cfg(test)]
mod force_full_tests {
    //! `force_full` is the stone restart-path canonicalizer: after body edits
    //! have accumulated into `incremental.lua` (overlaying a stale `main.lua`),
    //! it must fold the overlay back so the next `main.lua`-only process load is
    //! the complete current program. Gate: the produced `main.lua` is
    //! byte-identical to a from-scratch full build of the edited source, and the
    //! overlay file is gone.

    use super::SkullStone;
    use crate::skull_backend::SkullReload;

    /// `wipe_overlay` treats a MISSING overlay as fine (the normal "no overlay"
    /// case) but SURFACES any genuine removal failure -- a stale incremental.lua
    /// left beside a fresh main.lua is exactly the overlay/main desync we guard.
    #[test]
    fn wipe_overlay_missing_ok_real_failure_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No overlay on disk -> Ok.
        assert!(
            super::wipe_overlay(&dir.path().join("incremental.lua")).is_ok(),
            "a missing overlay must be Ok (nothing to wipe)"
        );
        // A real failure: the path is a directory, so remove_file can't remove it
        // and the error is NOT NotFound -> must surface as Err.
        let as_dir = dir.path().join("overlay_is_a_dir");
        std::fs::create_dir(&as_dir).expect("mkdir");
        assert!(
            super::wipe_overlay(&as_dir).is_err(),
            "a genuine overlay-removal failure must surface, not be swallowed"
        );
    }

    /// init at v1 -> body-edit to v2 (writes the overlay) -> force_full. The
    /// canonical `main.lua` must equal a fresh full build of v2, must DIFFER from
    /// the v1 baseline (proving the edit was actually folded in, not served
    /// stale), and `incremental.lua` must be wiped.
    #[test]
    fn force_full_folds_overlay_matches_fresh_and_wipes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\n";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }\n";

        // init at v1: writes main.lua(v1), wipes any overlay.
        std::fs::write(&rock, v1).unwrap();
        let mut skull = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init v1");
        let main_v1 = std::fs::read_to_string(&main_lua).expect("main.lua after init");
        assert!(!inc_lua.exists(), "init must leave no overlay");

        // Body edit to v2: a BodyOnly recompile writes the overlay; main.lua
        // stays the v1 baseline. Passing the changed file exercises the fast path
        // (re-read only `main.rock`); if it failed to pick up v2, the byte-identity
        // check against a fresh v2 build below would catch it.
        std::fs::write(&rock, v2).unwrap();
        let reload = skull.recompile(&[rock.clone()], &main_lua, &inc_lua).expect("recompile v2");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a const body edit should route to the overlay (Incremental tier)"
        );
        assert!(inc_lua.exists(), "body edit must produce an overlay");
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            main_v1,
            "a body edit must NOT touch main.lua (overlay carries the change)"
        );

        // force_full: fold the overlay back into main.lua, wipe the overlay.
        skull.force_full(&main_lua, &inc_lua).expect("force_full");
        let main_forced = std::fs::read_to_string(&main_lua).expect("main.lua after force_full");
        assert!(!inc_lua.exists(), "force_full must wipe the overlay");
        assert_ne!(
            main_forced, main_v1,
            "force_full must fold the v2 edit into main.lua, not serve the stale v1 baseline"
        );

        // Ground truth: a from-scratch full build of v2 (separate output path,
        // same on-disk source). force_full's main.lua must match it byte-for-byte.
        let fresh_main = src("fresh_main.lua");
        let fresh_inc = src("fresh_incremental.lua");
        SkullStone::init(&dir_str, &fresh_main, &fresh_inc).expect("fresh init v2");
        let main_fresh = std::fs::read_to_string(&fresh_main).expect("fresh main.lua");
        assert_eq!(
            main_forced, main_fresh,
            "force_full main.lua must be byte-identical to a fresh full build of the edited source"
        );
    }

    /// A project with a type error (a call to a non-existent function) must make
    /// `init` FAIL, not silently emit a partial `main.lua`. The bug: emit_full
    /// drops the un-checkable body but keeps its `__deco_init` registration, so
    /// the registered `func` is a nil global -> a baffling runtime nil-call
    /// instead of the clean compile error. `recompile` gates on
    /// `session.diagnostics()`; `init` must too.
    #[test]
    fn init_rejects_project_with_type_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // `boot` calls a function that does not exist -> bone collects a
        // diagnostic but Session::new still succeeds, so without the gate
        // emit_full produces a partial program.
        std::fs::write(&rock, "fn boot() { does_not_exist() }\n").unwrap();

        let res = SkullStone::init(&dir_str, &main_lua, &inc_lua);
        assert!(
            res.is_err(),
            "init must reject a project whose body references a non-existent function"
        );
        assert!(
            !main_lua.exists(),
            "init must NOT write a partial main.lua when the project has type errors"
        );
    }

    /// A recompile that hits a type error bails but leaves the session mutated
    /// into the broken state. The stone restart path then calls `force_full` to
    /// fold the overlay -- it must re-gate on diagnostics and refuse to write a
    /// partial `main.lua`, else the respawned luajit loads broken code.
    #[test]
    fn force_full_rejects_broken_session_after_failed_recompile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().unwrap().to_string();
        let src = |p: &str| dir.path().join(p);

        let main_lua = src("main.lua");
        let inc_lua = src("incremental.lua");
        let rock = src("main.rock");

        // init at a valid v1 -> main.lua(v1).
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\n";
        std::fs::write(&rock, v1).unwrap();
        let mut skull = SkullStone::init(&dir_str, &main_lua, &inc_lua).expect("init v1");
        let main_v1 = std::fs::read_to_string(&main_lua).expect("main.lua after init");

        // Edit to a broken v2 (calls a non-existent function). recompile bails,
        // but the session is now mutated into the broken state.
        let v2 = "fn helper() -> int { 3 }\nfn use_it() -> int { nope() }\n";
        std::fs::write(&rock, v2).unwrap();
        let reload = skull.recompile(&[rock.clone()], &main_lua, &inc_lua);
        assert!(reload.is_err(), "recompile must bail on the broken edit");

        // The restart fold must refuse rather than emit a partial program.
        let res = skull.force_full(&main_lua, &inc_lua);
        assert!(res.is_err(), "force_full must reject a broken session");
        assert_eq!(
            std::fs::read_to_string(&main_lua).unwrap(),
            main_v1,
            "force_full must leave the last-good main.lua untouched, not overwrite with a partial build"
        );
    }
}
