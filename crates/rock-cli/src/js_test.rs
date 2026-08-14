//! `rock test <path> --backend js` -- execute the `@test` suite under node via
//! the JS-subset emitter (`skull-codegen-js`), T1 of
//! `docs/multi-backend-testing-plan.md`.
//!
//! Shares the backend-neutral half of the test driver with the luajit path
//! (`skull_backend::skull_test`): unit discovery (`collect_test_units`), the
//! `@test` metadata (`skull::module_tests_with_ids`), the compile-failure
//! expect-failure fallback, and the `__SKULL__` protocol parser
//! (`test_protocol`). Per-backend here: per-test callee-closure emission
//! (`skull_codegen_js::emit::emit_with_closure`), the node batch runner, and
//! SKIP(reason) reporting -- a test whose closure hits anything outside the JS
//! subset reports as SKIP carrying the reject message, never FAIL, never
//! silently dropped. The end-of-run skip-reason histogram is the emitter work
//! list (T2+).

use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{parse_skull_markers, reason_histogram};
use crate::Result;
use colored::Colorize;
use skull_mir::mir::FuncRef;
use std::time::Instant;

/// One unit's (file or package) outcome under the JS executor.
/// Every reason one emission rejection carries: the emitter's primary plus its
/// siblings (independent findings from the same pass). One skipped test can owe
/// the subset several constructs.
fn all_reasons(e: &rock_core::CompilerError) -> Vec<String> {
    std::iter::once(e.message.clone())
        .chain(e.siblings.iter().map(|s| s.message.clone()))
        .collect()
}

enum JsUnitOutcome {
    /// bone accepted the unit; per-test verdicts follow.
    Ran {
        passed: usize,
        /// FAIL detail lines (throw under node, or a harness-invariant breach).
        fails: Vec<String>,
        /// (test name, EVERY skip reason) for tests outside the JS subset. All
        /// of them, not just the emitter's primary: the reason histogram is the
        /// subset's work list, and one reason per skipped test makes it a floor
        /// (the emitter computes the rest of the list in the same pass).
        skips: Vec<(String, Vec<String>)>,
    },
    /// bone rejected the unit and every `@test` expected the failure -- each
    /// counts as a passing test (luajit-path parity).
    ExpectFailPass { tests: usize },
    /// bone rejected the unit unexpectedly, or the emitted JS failed to load
    /// under node (a backend bug, not a test failure).
    UnitFail { reason: String, rendered: String },
}

/// Whole-run tallies -- the testable core consumed by the ratchet test.
pub struct JsSuiteSummary {
    pub files: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    pub unit_failures: usize,
    /// Skip reasons bucketed by exact message, count desc.
    pub skip_reasons: Vec<(String, usize)>,
}

/// `rock test --backend js` entry: run, print per-file lines + the end-of-run
/// summary and skip histogram, exit non-zero on any FAIL or unit failure.
pub fn js_test(paths: &[String]) -> Result<()> {
    let start = Instant::now();
    let summary = js_test_run(paths, true)?;
    println!(
        "\njs backend: {}/{} @tests passed ({} failed, {} skipped) across {} files ({:.2}s)",
        summary.tests_passed.to_string().green(),
        summary.tests_passed + summary.tests_failed + summary.tests_skipped,
        if summary.tests_failed == 0 {
            summary.tests_failed.to_string().green()
        } else {
            summary.tests_failed.to_string().red()
        },
        summary.tests_skipped.to_string().yellow(),
        summary.files,
        start.elapsed().as_secs_f64(),
    );
    if summary.unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build/run (see FAILED lines above)",
            summary.unit_failures.to_string().red()
        );
    }
    if !summary.skip_reasons.is_empty() {
        println!("\n{}", "=== skip reasons (js) -- the T2+ work list ===".bold());
        let shown = summary.skip_reasons.len().min(50);
        for (reason, count) in summary.skip_reasons.iter().take(shown) {
            println!("  {:>5}  {}", count.to_string().yellow(), reason);
        }
        if summary.skip_reasons.len() > shown {
            println!(
                "  {:>5}  ({} more distinct reasons)",
                "...",
                summary.skip_reasons.len() - shown
            );
        }
    }
    // Before the exit below, for the same reason `skull_test` reports here:
    // `process::exit` runs no destructor and never returns to `main`.
    let ablation_fatals = crate::ablation_report::report("rock test --backend js");

    if summary.tests_failed > 0 || summary.unit_failures > 0 || ablation_fatals {
        // Plain exit: the census above already drained the registry.
        std::process::exit(1);
    }
    Ok(())
}

/// Run the suite; optionally print per-file lines as units complete. Public for
/// the ratchet test (`print = false`).
pub fn js_test_run(paths: &[String], print: bool) -> Result<JsSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    let mut summary = JsSuiteSummary {
        files: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        unit_failures: 0,
        skip_reasons: Vec::new(),
    };
    let mut all_skip_reasons: Vec<String> = Vec::new();

    // bone can panic during inference on some inputs (a bone bug); contain it
    // per unit like the luajit driver does.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut outcomes: Vec<(String, std::thread::Result<JsUnitOutcome>)> = Vec::new();
    for file in &files {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            js_build_file(file)
        }));
        outcomes.push((short_path(file), outcome));
    }
    for dir in &package_dirs {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            js_build_package(dir)
        }));
        outcomes.push((short_path(dir), outcome));
    }

    std::panic::set_hook(prev_hook);

    for (label, outcome) in outcomes {
        summary.files += 1;
        match outcome {
            Ok(JsUnitOutcome::Ran { passed, fails, skips }) => {
                summary.tests_passed += passed;
                summary.tests_failed += fails.len();
                summary.tests_skipped += skips.len();
                for (_, reasons) in &skips {
                    all_skip_reasons.extend(reasons.iter().cloned());
                }
                if print && (passed + fails.len() + skips.len() > 0) {
                    let status = if fails.is_empty() { "ok".green() } else { "FAILED".red() };
                    println!(
                        "{:.<60} {}  ({} pass, {} fail, {} skip)",
                        label,
                        status,
                        passed,
                        fails.len(),
                        skips.len()
                    );
                    for detail in &fails {
                        println!("  {} {}", "FAIL".red(), detail);
                    }
                }
            }
            Ok(JsUnitOutcome::ExpectFailPass { tests }) => {
                summary.tests_passed += tests;
                if print {
                    println!(
                        "{:.<60} {}  ({} pass, 0 fail, 0 skip)",
                        label,
                        "ok".green(),
                        tests
                    );
                }
            }
            Ok(JsUnitOutcome::UnitFail { reason, rendered }) => {
                summary.unit_failures += 1;
                if print {
                    println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        println!("{rendered}");
                    }
                }
            }
            Err(panic) => {
                summary.unit_failures += 1;
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                if print {
                    println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
                }
            }
        }
    }

    summary.skip_reasons = reason_histogram(all_skip_reasons.iter().map(|s| s.as_str()));
    Ok(summary)
}

/// One isolated test file: parse + bone type-check with the stdlib, discover
/// `@test` fns, then run them via the per-test closure emission + node batch.
fn js_build_file(file: &str) -> JsUnitOutcome {
    let src = match fs_err::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            return JsUnitOutcome::UnitFail { reason: e.to_string(), rendered: e.to_string() }
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(file)) {
        Ok(u) => u,
        Err(e) => return js_compile_failure(file, &src, &[e]),
    };
    let user_file = user.file;
    let mut ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => return js_compile_failure(file, &src, &errs),
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(file)) {
        return JsUnitOutcome::UnitFail {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {file}: {e:#}"),
        };
    }
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    // A discovery error (malformed `@test` annotation on a bone-accepted file)
    // is a rejection, never an expected compile failure -- so it does NOT route
    // through `js_compile_failure`'s expect-failure escape.
    let tests: Vec<(FuncRef, skull::SkullTest)> = match skull::module_tests_with_ids(&ws, user_mid)
    {
        Ok(tests) => tests
            .into_iter()
            .map(|(fid, t)| (FuncRef { module: user_mid, func: fid }, t))
            .collect(),
        Err(e) => {
            return JsUnitOutcome::UnitFail {
                reason: categorize(&e.message),
                rendered: crate::skull_backend::render_file_errs(file, &src, &[e]),
            }
        }
    };
    js_run_tests(&ws, &tests)
}

/// A `stick.toml` package built as ONE workspace (tests included), mirroring
/// `skull_build_package`; its `@test`s run through the same per-test path.
fn js_build_package(dir: &str) -> JsUnitOutcome {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            return JsUnitOutcome::UnitFail {
                reason: "cannot load project".to_string(),
                rendered: e.to_string(),
            }
        }
    };
    let (mut modules, _entry) = match crate::skull_backend::skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            return JsUnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            }
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => {
            return JsUnitOutcome::UnitFail {
                reason: categorize(&errs[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errs),
            }
        }
    };
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir)) {
        return JsUnitOutcome::UnitFail {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {dir}: {e:#}"),
        };
    }
    let tests = match crate::skull_backend::collect_package_test_fns(&ws, dir) {
        Ok(tests) => tests,
        Err(e) => {
            return JsUnitOutcome::UnitFail {
                reason: categorize(&e.message),
                rendered: e.message,
            }
        }
    };
    js_run_tests(&ws, &tests)
}

/// The luajit driver's compile-failure rule, shared verbatim: a bone rejection
/// where every discovered `@test` carries `expect_failure`/`@ignore` is the
/// expected (passing) outcome; anything else is a unit failure.
fn js_compile_failure(
    file: &str,
    src: &str,
    errs: &[rock_core::CompilerError],
) -> JsUnitOutcome {
    if let Some(tests) = crate::skull_backend::all_tests_expect_compile_failure(file, src) {
        return JsUnitOutcome::ExpectFailPass { tests };
    }
    JsUnitOutcome::UnitFail {
        reason: categorize(&errs[0].message),
        rendered: crate::skull_backend::render_file_errs(file, src, errs),
    }
}

/// Per-test closure emission + one node invocation for the unit's runnable
/// tests, speaking the `__SKULL__` protocol.
fn js_run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
) -> JsUnitOutcome {
    let mut fails: Vec<String> = Vec::new();
    let mut skips: Vec<(String, Vec<String>)> = Vec::new();

    // (test index, per-test isolated defs) for tests emittable in ISOLATION.
    let mut supported: Vec<(usize, String)> = Vec::new();
    // Test indices that reject ONLY because they read a decorator-populated
    // global (the module-load side effect the per-test closure omits) -- lifted
    // below by a whole-file emission that runs the applications once.
    let mut deco_pending: Vec<usize> = Vec::new();
    for (i, (fr, t)) in tests.iter().enumerate() {
        let js_name = format!("__t{i}");
        let roots = [(*fr, js_name)];
        let emitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            skull_codegen_js::emit::emit_with_closure(ws, &roots)
        }));
        match emitted {
            Ok(Ok(js)) => supported.push((i, js)),
            Ok(Err(e)) if is_decorator_populated_reject(&e.message) => deco_pending.push(i),
            Ok(Err(e)) => skips.push((t.name.clone(), all_reasons(&e))),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                skips.push((t.name.clone(), vec![format!("panic during JS emission: {msg}")]));
            }
        }
    }

    // Assemble the shared defs + the runnable test index list (test order).
    //
    // The per-test emission above is the CLASSIFIER -- it decides, one test at a
    // time, which tests the JS subset can serve, so an unsupported one skips by
    // itself instead of taking the file down with it. Assembly is a separate
    // question, and the answer is always ONE program over the surviving roots:
    // concatenating the per-test closures gives every block its own copy of the
    // global initializers, so a global derived from shared mutable state is
    // recomputed per block and comes out wrong. That is the model luajit and
    // `rock run --backend js` already use, and it is what makes the Session
    // Arena registry (four arena iteration tests) agree across backends.
    let mut run_idxs: Vec<usize> = supported.iter().map(|(i, _)| *i).collect();
    run_idxs.extend(deco_pending.iter().copied());
    run_idxs.sort_unstable();
    let shared_defs = if run_idxs.is_empty() {
        String::new()
    } else {
        let roots: Vec<(FuncRef, String)> =
            run_idxs.iter().map(|&i| (tests[i].0, format!("__t{i}"))).collect();
        match skull_codegen_js::emit::emit_program_with_roots(ws, &roots) {
            Ok(blob) => blob,
            Err(e) => {
                // Whole-file emission rejected for something no single test hit
                // on its own (a deco-dependent test that also reaches an
                // unsupported construct, or a construct only the shared program
                // reaches): keep the individually-supported tests by falling back
                // to the concatenation, and skip the deco-dependent ones.
                for &i in &deco_pending {
                    skips.push((tests[i].1.name.clone(), all_reasons(&e)));
                }
                run_idxs = supported.iter().map(|(i, _)| *i).collect();
                supported.iter().map(|(_, d)| d.as_str()).collect::<String>()
            }
        }
    };

    if run_idxs.is_empty() {
        return JsUnitOutcome::Ran { passed: 0, fails, skips };
    }

    // (display name, root JS name, expected output, value test?) per runnable test.
    let meta: Vec<(String, String, Option<String>, bool)> = run_idxs
        .iter()
        .map(|&i| {
            let t = &tests[i].1;
            (t.name.clone(), format!("__t{i}"), t.output.clone(), t.returns.is_some())
        })
        .collect();

    let program = build_node_program(&shared_defs, &meta);
    let timeout = Some(std::time::Duration::from_secs(15));
    let run = match rock_host::js_runner::run_node_code_with_timeout(&program, timeout) {
        Ok(r) => r,
        Err(e) => {
            return JsUnitOutcome::UnitFail {
                reason: "node run failed".to_string(),
                rendered: e.message,
            }
        }
    };

    // The harness try/catches every test, so marker lines exist iff the program
    // loaded. Missing/short markers = the emitted JS itself is bad (a backend
    // bug), surfaced with the program for diagnosis -- never silently absorbed.
    let markers = match parse_skull_markers(&run.stdout) {
        Some(m) if m.passed + m.fails.len() + m.values.len() == meta.len() => m,
        _ => {
            return JsUnitOutcome::UnitFail {
                reason: "invalid JS emitted (js backend bug)".to_string(),
                rendered: format!(
                    "stdout:\n{}\nstderr:\n{}\n\n--- emitted js (truncated) ---\n{}",
                    run.stdout,
                    run.stderr,
                    truncate_js(&program)
                ),
            }
        }
    };
    fails.extend(markers.fails);
    let mut passed = markers.passed;

    // VALUE lines: compare the serialized return value HOST-side against the
    // pinned `@test(returns = ...)` literal -- same policy as the luajit path
    // (test_protocol::compare_value is the single comparison site).
    let expects: std::collections::HashMap<&str, &skull::ValueExpect> = tests
        .iter()
        .filter_map(|(_, t)| t.returns.as_ref().map(|v| (t.name.as_str(), v)))
        .collect();
    for (name, val) in &markers.values {
        let Some(expect) = expects.get(name.as_str()) else {
            return JsUnitOutcome::UnitFail {
                reason: "harness invariant breach (js backend bug)".to_string(),
                rendered: format!("VALUE line for `{name}` without a returns expectation"),
            };
        };
        match crate::test_protocol::compare_value(expect, val, crate::test_protocol::FLOAT_REL_TOL)
        {
            Ok(()) => passed += 1,
            Err(detail) => fails.push(format!("{name}: {detail}")),
        }
    }
    JsUnitOutcome::Ran { passed, fails, skips }
}

/// `rock run <file> --backend js`: emit the whole file (+ stdlib) as ONE node
/// program via `emit_program`, prepend the runtime preludes + a DIRECT
/// (unbuffered) `print`/`put`, append `<main>();` when the entry module defines a
/// nullary `main` (D3), and STREAM it through node with no timeout (D5),
/// propagating node's exit code. A bone rejection or emit reject prints the
/// diagnostic and exits non-zero -- never a silent fallback.
pub fn js_run(path: &str) -> Result<()> {
    let emitted = if std::path::Path::new(path).is_dir() {
        emit_js_program_project(path).0
    } else {
        emit_js_program_single(path)
    };

    let program = build_run_program(&emitted);
    match rock_host::js_runner::run_node_code_streaming(&program) {
        Ok(0) => Ok(()),
        Ok(code) => crate::ablation_report::exit_with_census(code),
        Err(e) => {
            log::error!(target: "JS", "node run failed: {}", e.message);
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// Emit the whole program for a single `.rock` file (M1 headless path). Diagnoses
/// and exits on any parse/check/emit rejection -- never a silent fallback.
fn emit_js_program_single(path: &str) -> skull_codegen_js::emit::JsProgram {
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "JS", "cannot read {}: {}", path, e);
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(path)) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}", crate::skull_backend::render_file_errs(path, &src, &[e]));
            crate::ablation_report::exit_with_census(1);
        }
    };
    let user_file = user.file;
    let mut ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", crate::skull_backend::render_file_errs(path, &src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    };
    // Single-file runs resolve from the file's enclosing project (walk-up), so
    // a file inside a [build.vars] project behaves the same on both backends.
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        log::error!(target: "JS", "build var resolution failed for {}: {:#}", path, e);
        crate::ablation_report::exit_with_census(1);
    }
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    match skull_codegen_js::emit::emit_program(&ws, user_mid) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", crate::skull_backend::render_file_errs(path, &src, &[e]));
            crate::ablation_report::exit_with_census(1);
        }
    }
}

/// Emit the whole program for a project directory (`rock run <dir> --backend
/// js`). Mirrors `skull_emit_project`'s front half: discover the deterministic
/// module set, bone-check, resolve the entry module, then the SAME
/// `emit_program` the single-file path runs (module-loop-driven, order fixed by
/// `project.rs` file-name sort). Game projects drop test-only files. Any
/// rejection prints the project diagnostic and exits. Also returns the checked
/// workspace -- the web assembler collects `rock_shader` pairs from it.
pub(crate) fn emit_js_program_project(
    dir: &str,
) -> (skull_codegen_js::emit::JsProgram, bone_semantic::inference::Workspace) {
    match try_emit_js_program_project(dir) {
        Ok(emitted) => emitted,
        Err(e) => {
            eprintln!("{e}");
            crate::ablation_report::exit_with_census(1);
        }
    }
}

pub(crate) fn try_emit_js_program_project(
    dir: &str,
) -> Result<(skull_codegen_js::emit::JsProgram, bone_semantic::inference::Workspace)> {
    let (program, _cache, ws) = try_emit_js_program_project_cached(dir)?;
    Ok((program, ws))
}

pub(crate) fn try_emit_js_program_project_cached(
    dir: &str,
) -> Result<(
    skull_codegen_js::emit::JsProgram,
    skull_codegen_js::emit::JsEmitCache,
    bone_semantic::inference::Workspace,
)> {
    use std::path::PathBuf;
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec)
        .map_err(|e| anyhow::anyhow!("cannot load project {dir}: {e}"))?;
    let sources = if rock_host::project::is_game_project(&PathBuf::from(dir)).unwrap_or(false) {
        sources.production_only()
    } else {
        sources
    };
    let (mut modules, entry_file) = match crate::skull_backend::skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => return Err(anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_errs(&sources, &errs)
        )),
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => return Err(anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_errs(&sources, &errs)
        )),
    };
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir))?;
    let user_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    let (prog, cache) = match skull_codegen_js::emit::emit_program_cached(&ws, user_mid) {
        Ok(p) => p,
        Err(e) => return Err(anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_err(&sources, &e)
        )),
    };
    Ok((prog, cache, ws))
}

/// Node-specific glue for the engine-agnostic RNG prelude (`JS_RNG_RUNTIME`) and
/// `env_var`: `process.env` is node-only, so it lives in the DRIVER, not a
/// prelude const (D3/D4, a browser bundle swaps a config object at M3). Seeding
/// mirrors Lua's per-process rule (`crates/skull/src/lib.rs`): one seed after the
/// preludes -- `ROCK_TEST_SEED` when set+finite else `Date.now()`; a test needing
/// determinism calls `random_seed()` itself. `__rock_env_var` maps an unset var
/// to Rock null (`undefined -> null`), matching `os.getenv`.
const NODE_RNG_ENV_GLUE: &str = r#"
function __rock_env_var(n) { var v = process.env[n]; return v === undefined ? null : v; }
(function() { var s = Number(process.env.ROCK_TEST_SEED); __rock_rng_seed(Number.isFinite(s) ? s : Date.now()); })();
"#;

/// Assemble the streaming run program: the same runtime prelude set/order as the
/// test harness (`build_node_program`), a scoped `var __G = {}`, a DIRECT
/// `print`/`put` (unbuffered `process.stdout.write`, no `__buf`/`__SKULL__`
/// protocol), the whole emitted program, then `<main>();` when the entry defines
/// one. No `__rock_main_wrapper`: its HOT_RELOAD guard + Ctrl+C pcall are engine-
/// specific (D3, revisited at M3) -- a bare call matches luajit's single-file path.
fn build_run_program(emitted: &skull_codegen_js::emit::JsProgram) -> String {
    let mut p = String::from(skull_codegen_js::JS_NB_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_VEC_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_TABLE_PRIM);
    p.push_str(skull_codegen_js::JS_MAP_ITER_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRUCTURAL_EQ);
    p.push_str(skull_codegen_js::JS_ENUM_RUNTIME);
    p.push_str(skull_codegen_js::JS_CALL_HELPER);
    p.push_str(skull_codegen_js::JS_CORO_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRING_RUNTIME);
    p.push_str(skull_codegen_js::JS_INSPECT_RUNTIME);
    p.push_str(skull_codegen_js::JS_DEEPCOPY_RUNTIME);
    p.push_str(skull_codegen_js::JS_LOG_RUNTIME);
    p.push_str(skull_codegen_js::JS_RNG_RUNTIME);
    p.push_str(NODE_RNG_ENV_GLUE);
    p.push_str(
        r#"
var __G = {};
function print() {
  var p = [];
  for (var i = 0; i < arguments.length; i++) p.push(lua_tostring(arguments[i]));
  process.stdout.write(p.join("\t") + "\n");
}
function put(s) { process.stdout.write(s); }
"#,
    );
    p.push_str(&emitted.program);
    if let Some(main) = &emitted.main_js_name {
        p.push_str(&format!("\n{main}();\n"));
    }
    p
}

/// A JS double-quoted string literal for a Rust string, escaping the control
/// and non-ASCII chars a JS lexer needs (test output can carry arbitrary
/// bytes). `{:?}` would emit Rust's `\u{XXXX}`, which JS rejects.
pub(crate) fn js_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                for u in c.encode_utf16(&mut [0u16; 2]) {
                    o.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// True for the per-test reject a decorator-populated global read raises
/// (`emit.rs` `discover_globals`) -- the marker that a whole-file emission (which
/// runs the applications once) would lift the test.
fn is_decorator_populated_reject(msg: &str) -> bool {
    msg.contains("is populated by decorator application")
}

/// Assemble the per-file node program: shim + string-runtime preamble, a
/// buffer-capturing `print` (Lua-parity: args `lua_tostring`-joined by TAB, one
/// trailing newline per call), the shared emitted defs (per-test isolated
/// closures concatenated, or one whole-file program when decorator applications
/// must run at load), then the harness running each test with its buffer reset
/// and comparing the captured stdout (trimmed) against the `output=` expectation
/// -- one `__SKULL__` line per test.
fn build_node_program(
    shared_defs: &str,
    tests: &[(String, String, Option<String>, bool)],
) -> String {
    let mut p = String::from(skull_codegen_js::JS_NB_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_VEC_CHECK_SHIMS);
    p.push_str(skull_codegen_js::JS_TABLE_PRIM);
    p.push_str(skull_codegen_js::JS_MAP_ITER_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRUCTURAL_EQ);
    p.push_str(skull_codegen_js::JS_ENUM_RUNTIME);
    p.push_str(skull_codegen_js::JS_CALL_HELPER);
    p.push_str(skull_codegen_js::JS_CORO_RUNTIME);
    p.push_str(skull_codegen_js::JS_STRING_RUNTIME);
    p.push_str(skull_codegen_js::JS_INSPECT_RUNTIME);
    p.push_str(skull_codegen_js::JS_DEEPCOPY_RUNTIME);
    p.push_str(skull_codegen_js::JS_LOG_RUNTIME);
    p.push_str(skull_codegen_js::JS_RNG_RUNTIME);
    p.push_str(NODE_RNG_ENV_GLUE);
    p.push_str(
        r#"
var __G = {};
var __buf = [];
function print() {
  var p = [];
  for (var i = 0; i < arguments.length; i++) p.push(lua_tostring(arguments[i]));
  __buf.push(p.join("\t") + "\n");
}
// `put` = the test-local `io.write` idiom: append raw, no separators/newline.
function put(s) { __buf.push(s); }
"#,
    );
    p.push_str(shared_defs);
    p.push_str("var __tests = [\n");
    for (name, js_name, out, is_value) in tests {
        let out_js = match out {
            Some(s) => js_str(s),
            None => "null".to_string(),
        };
        p.push_str(&format!(
            "  {{ f: {js_name}, name: {}, out: {out_js}, val: {is_value} }},\n",
            js_str(name)
        ));
    }
    p.push_str("];\n");
    // __fmtval: toPrecision(17) round-trips any f64 (the host compares parsed
    // f64s, so it need not match luajit's %.17g byte-for-byte); bools as-is.
    p.push_str(
        r#"function __esc(s) { return String(s).replace(/\\/g, "\\\\").replace(/\n/g, "\\n").replace(/\t/g, "\\t"); }
function __fmtval(v) {
  if (typeof v === "boolean") return v ? "true" : "false";
  return Number(v).toPrecision(17);
}
for (var i = 0; i < __tests.length; i++) {
  var t = __tests[i];
  __buf = [];
  try {
    var ret = t.f();
    // Only TRAILING whitespace stripped (parity with the luajit harness): a
    // leading-padded `{x:5}` -> `   42` stays significant for @test(output=..).
    var got = __buf.join("").replace(/\s+$/, "");
    if (t.out !== null && got !== t.out) {
      process.stdout.write("\n__SKULL__\tFAIL\t" + t.name + ": output mismatch: expected <" + __esc(t.out) + "> got <" + __esc(got) + ">\n");
    } else if (t.val) {
      process.stdout.write("\n__SKULL__\tVALUE\t" + t.name + "\t" + __fmtval(ret) + "\n");
    } else {
      process.stdout.write("\n__SKULL__\tPASS\t\n");
    }
  } catch (e) {
    var m = (e && e.message !== undefined) ? e.message : e;
    process.stdout.write("\n__SKULL__\tFAIL\t" + t.name + ": error: " + __esc(m) + "\n");
  }
}
"#,
    );
    p
}

/// Keep emitted-JS dumps in error reports manageable.
fn truncate_js(s: &str) -> String {
    if s.len() > 4000 {
        format!("{}\n... ({} more bytes)", &s[..4000], s.len() - 4000)
    } else {
        s.to_string()
    }
}
