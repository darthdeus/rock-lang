//! `rock test <path> --backend js` -- execute the `@test` suite under node via
//! the JS-subset emitter (`skull-codegen-js`).
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
//! list.

use crate::Result;
use crate::test_protocol::{parse_skull_markers, reason_histogram};
use crate::test_unit::{UnitCheck, UnitFailure};
use bone_core::prelude::Instant;
use colored::Colorize;
use skull_mir::mir::FuncRef;

/// Every reason one emission rejection carries: the emitter's primary plus its
/// siblings (independent findings from the same pass). One skipped test can owe
/// the subset several constructs.
fn all_reasons(e: &rock_core::CompilerError) -> Vec<String> {
    std::iter::once(e.message.clone())
        .chain(e.siblings.iter().map(|s| s.message.clone()))
        .collect()
}

/// The verdicts of one unit (file or package) bone accepted, or whose every
/// `@test` expected the rejection (each then counts as passing).
#[derive(Default)]
struct JsUnitRun {
    passed: usize,
    /// FAIL detail lines (throw under node, or a harness-invariant breach).
    fails: Vec<String>,
    /// (test name, EVERY skip reason) for tests outside the JS subset. All
    /// of them, not just the emitter's primary: the reason histogram is the
    /// subset's work list, and one reason per skipped test makes it a floor
    /// (the emitter computes the rest of the list in the same pass).
    skips: Vec<(String, Vec<String>)>,
    /// Tests `@test(backends = ...)` limits to other backends.
    other_backend: usize,
}

/// A unit bone rejected unexpectedly, or whose emitted JS failed to load
/// under node (a backend bug, not a test failure).
type JsUnitOutcome = std::result::Result<JsUnitRun, UnitFailure>;

/// Whole-run tallies.
pub struct JsSuiteSummary {
    pub files: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    /// Tests `@test(backends = ...)` limits to other backends: never emitted,
    /// never run, and not skips (the CI skip cap measures only JS gaps).
    pub tests_other_backend: usize,
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
        "\njs backend: {}/{} @tests passed ({} failed, {} skipped, {} not for js) across {} files ({:.2}s)",
        summary.tests_passed.to_string().green(),
        summary.tests_passed + summary.tests_failed + summary.tests_skipped,
        if summary.tests_failed == 0 {
            summary.tests_failed.to_string().green()
        } else {
            summary.tests_failed.to_string().red()
        },
        summary.tests_skipped.to_string().yellow(),
        summary.tests_other_backend,
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
        println!(
            "\n{}",
            "=== skip reasons (js) -- the emitter work list ===".bold()
        );
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
    if summary.tests_failed > 0 || summary.unit_failures > 0 {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Run the suite; optionally print per-file lines as units complete.
pub fn js_test_run(paths: &[String], print: bool) -> Result<JsSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    let mut summary = JsSuiteSummary {
        files: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        tests_other_backend: 0,
        unit_failures: 0,
        skip_reasons: Vec::new(),
    };
    let mut all_skip_reasons: Vec<String> = Vec::new();

    let outcomes = crate::test_unit::build_units(&files, &package_dirs, js_build_unit);

    for (label, outcome) in outcomes {
        summary.files += 1;
        match outcome {
            Ok(Ok(JsUnitRun {
                passed,
                fails,
                skips,
                other_backend,
            })) => {
                summary.tests_passed += passed;
                summary.tests_failed += fails.len();
                summary.tests_skipped += skips.len();
                summary.tests_other_backend += other_backend;
                for (_, reasons) in &skips {
                    all_skip_reasons.extend(reasons.iter().cloned());
                }
                if print && (passed + fails.len() + skips.len() + other_backend > 0) {
                    let status = if fails.is_empty() {
                        "ok".green()
                    } else {
                        "FAILED".red()
                    };
                    let elsewhere = if other_backend > 0 {
                        format!(", {other_backend} not for js")
                    } else {
                        String::new()
                    };
                    println!(
                        "{:.<60} {}  ({} pass, {} fail, {} skip{})",
                        label,
                        status,
                        passed,
                        fails.len(),
                        skips.len(),
                        elsewhere
                    );
                    for detail in &fails {
                        println!("  {} {}", "FAIL".red(), detail);
                    }
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        for (name, reasons) in &skips {
                            println!("  {} {}: {}", "SKIP".yellow(), name, reasons.join(" | "));
                        }
                    }
                }
            }
            Ok(Err(failure)) => {
                summary.unit_failures += 1;
                if print {
                    failure.print(&label);
                }
            }
            Err(panic) => {
                summary.unit_failures += 1;
                if print {
                    crate::test_unit::print_unit_panic(&label, &*panic);
                }
            }
        }
    }

    summary.skip_reasons = reason_histogram(all_skip_reasons.iter().map(|s| s.as_str()));
    Ok(summary)
}

/// One test unit (a file, or a package built as ONE workspace with its tests),
/// its `@test`s run via per-test closure emission and one node batch.
fn js_build_unit(unit: &str, is_package: bool) -> JsUnitOutcome {
    if let Err(error) = (crate::registry::RuntimeServices::registered().prepare_unit)(unit) {
        return Err(UnitFailure {
            reason: "sound bank preparation failed".to_string(),
            rendered: format!("sound bank preparation failed for {unit}: {error:#}"),
        });
    }
    let checked = if is_package {
        crate::test_unit::check_package(unit, true)
    } else {
        crate::test_unit::check_file(unit, true)
    };
    match checked? {
        UnitCheck::Checked { ws, tests, sources } => js_run_tests(&ws, &tests, &|e| {
            crate::skull_backend::render_project_errs(&sources, std::slice::from_ref(e))
        }),
        UnitCheck::ExpectedFailure(tests) => Ok(JsUnitRun {
            passed: tests,
            ..JsUnitRun::default()
        }),
    }
}

/// Per-test closure emission + one node invocation for the unit's tests that
/// apply to JS, speaking the `__SKULL__` protocol. A test limited by
/// `@test(backends = ...)` to other backends is neither emitted nor run; it is
/// counted apart so it never reads as a JS gap.
fn js_run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    render: &dyn Fn(&rock_core::CompilerError) -> String,
) -> JsUnitOutcome {
    let (tests, elsewhere): (Vec<_>, Vec<_>) = tests
        .iter()
        .cloned()
        .partition(|(_, t)| t.runs_on(bone_core::annotations::TestBackend::Js));
    let tests = tests.as_slice();
    let other_backend = elsewhere.len();
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
            Ok(Err(e)) if e.kind == rock_core::errors::CompilerErrorKind::TargetLimit => {
                return Err(UnitFailure {
                    reason: e.message.clone(),
                    rendered: render(&e),
                });
            }
            Ok(Err(e)) => skips.push((t.name.clone(), all_reasons(&e))),
            Err(panic) => {
                let msg = crate::common::panic_message(&*panic);
                skips.push((
                    t.name.clone(),
                    vec![format!("panic during JS emission: {msg}")],
                ));
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
    let (shared_defs, shared_has_code_refs) = if run_idxs.is_empty() {
        (String::new(), false)
    } else {
        let roots: Vec<(FuncRef, String)> = run_idxs
            .iter()
            .map(|&i| (tests[i].0, format!("__t{i}")))
            .collect();
        match skull_codegen_js::emit::emit_program_with_roots_artifact(ws, &roots) {
            Ok((blob, artifact)) => {
                let has_code_refs = artifact.entries.values().any(|entry| entry.escaping);
                (blob, has_code_refs)
            }
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
                let blob = supported
                    .iter()
                    .map(|(_, d)| d.as_str())
                    .collect::<String>();
                let has_code_refs = blob.contains("__rock_code_slot_ref(");
                (blob, has_code_refs)
            }
        }
    };

    if run_idxs.is_empty() {
        return Ok(JsUnitRun {
            passed: 0,
            fails,
            skips,
            other_backend,
        });
    }

    // (display name, root JS name, expected output, value test?) per runnable test.
    let meta: Vec<(String, String, Option<String>, bool)> = run_idxs
        .iter()
        .map(|&i| {
            let t = &tests[i].1;
            (
                t.name.clone(),
                format!("__t{i}"),
                t.output.clone(),
                t.returns.is_some(),
            )
        })
        .collect();

    let program = build_node_program(&shared_defs, &meta, shared_has_code_refs);
    let timeout = std::time::Duration::from_secs(15);
    let run = match crate::js_runner::run_node_code_with_timeout(&program, timeout) {
        Ok(r) => r,
        Err(e) => {
            return Err(UnitFailure {
                reason: "node run failed".to_string(),
                rendered: e.message,
            });
        }
    };

    // The harness try/catches every test, so marker lines exist iff the program
    // loaded. Missing/short markers = the emitted JS itself is bad (a backend
    // bug), surfaced with the program for diagnosis -- never silently absorbed.
    let markers = match parse_skull_markers(&run.stdout) {
        Some(m) if m.passed + m.fails.len() + m.values.len() == meta.len() => m,
        _ if run.stderr.contains("Maximum call stack size exceeded") => {
            return Err(UnitFailure {
                reason: "emitted program too deeply nested for node's parser".to_string(),
                rendered: run.stderr,
            });
        }
        _ => {
            return Err(UnitFailure {
                reason: "invalid JS emitted (js backend bug)".to_string(),
                rendered: format!(
                    "stdout:\n{}\nstderr:\n{}\n\n--- emitted js (truncated) ---\n{}",
                    run.stdout,
                    run.stderr,
                    crate::skull_backend::program_excerpt(&[&program])
                ),
            });
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
            return Err(UnitFailure {
                reason: "harness invariant breach (js backend bug)".to_string(),
                rendered: format!("VALUE line for `{name}` without a returns expectation"),
            });
        };
        match crate::test_protocol::compare_value(expect, val, crate::test_protocol::FLOAT_REL_TOL)
        {
            Ok(()) => passed += 1,
            Err(detail) => fails.push(format!("{name}: {detail}")),
        }
    }
    Ok(JsUnitRun {
        passed,
        fails,
        skips,
        other_backend,
    })
}

/// `rock run <file> --backend js`: emit the whole file (+ stdlib) as ONE node
/// program via `emit_program`, prepend the runtime preludes + a DIRECT
/// (unbuffered) `print`/`put`, append `<main>();` when the entry module defines a
/// nullary `main`, and STREAM it through node with no timeout (a real program
/// can run arbitrarily long), propagating node's exit code. A bone rejection or
/// emit reject prints the diagnostic and exits non-zero -- never a silent fallback.
pub(crate) fn js_run(path: &str) -> Result<()> {
    let emitted = if std::path::Path::new(path).is_dir() {
        emit_js_program_project(path)
    } else {
        emit_js_program_single(path)
    };

    let program = build_run_program(&emitted);
    match crate::js_runner::run_node_code_streaming(&program) {
        Ok(0) => Ok(()),
        Ok(code) => crate::invocation_log::exit(code),
        Err(e) => {
            log::error!(target: "JS", "node run failed: {}", e.message);
            crate::invocation_log::exit(1);
        }
    }
}

/// Emit the whole program for a single `.rock` file. Diagnoses
/// and exits on any parse/check/emit rejection -- never a silent fallback.
fn emit_js_program_single(path: &str) -> skull_codegen_js::emit::JsProgram {
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "JS", "cannot read {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(path)) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "{}",
                crate::skull_backend::render_file_errs(path, &src, &[e])
            );
            crate::invocation_log::exit(1);
        }
    };
    let user_file = user.file;
    let mut ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!(
                "{}",
                crate::skull_backend::render_file_errs(path, &src, &errs)
            );
            crate::invocation_log::exit(1);
        }
    };
    // Single-file runs resolve from the file's enclosing project (walk-up), so
    // a file inside a [build.vars] project behaves the same on both backends.
    if let Err(e) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(path)) {
        log::error!(target: "JS", "build var resolution failed for {}: {:#}", path, e);
        crate::invocation_log::exit(1);
    }
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    match skull_codegen_js::emit::emit_program(&ws, user_mid) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}",
                crate::skull_backend::render_file_errs(path, &src, &[e])
            );
            crate::invocation_log::exit(1);
        }
    }
}

/// Emit the whole program for a project directory (`rock run <dir> --backend
/// js`): discover the deterministic module set, bone-check, resolve the entry
/// module, then emit the whole program as the single-file path does. Game
/// projects drop test-only files. Any rejection prints the project diagnostic
/// and exits.
fn emit_js_program_project(dir: &str) -> skull_codegen_js::emit::JsProgram {
    match try_emit_js_program_project(dir) {
        Ok(emitted) => emitted,
        Err(e) => {
            eprintln!("{e}");
            crate::invocation_log::exit(1);
        }
    }
}

fn try_emit_js_program_project(dir: &str) -> Result<skull_codegen_js::emit::JsProgram> {
    // A game builds without its tests, like the luajit compile.
    let is_game =
        (crate::registry::RuntimeServices::registered().is_game_project)(std::path::Path::new(dir))
            .unwrap_or(false);
    let sources = crate::live_project::project_sources(dir, !is_game)
        .map_err(|e| anyhow::anyhow!("cannot load project {dir}: {e}"))?;
    let (mut ws, entry_file) = crate::skull_backend::check_project(&sources).map_err(|errs| {
        anyhow::anyhow!(
            "{}",
            crate::skull_backend::render_project_errs(&sources, &errs)
        )
    })?;
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir))?;
    let user_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    // The cached emission also collects the project's shaders, so a project
    // whose shaders do not collect is rejected here, unlike a single file.
    skull_codegen_js::emit::emit_program_cached(&ws, user_mid)
        .map(|(program, _cache)| program)
        .map_err(|e| anyhow::anyhow!("{}", crate::skull_backend::render_project_err(&sources, &e)))
}

/// The streaming run program: the shipped prelude over node's host (direct,
/// unbuffered writes), the whole emitted program, then `<main>();` when the
/// entry defines one. A bare call matches luajit's single-file path.
fn build_run_program(emitted: &skull_codegen_js::emit::JsProgram) -> String {
    let mut p = skull_codegen_js::assemble_prelude(
        &emitted.program,
        emitted.has_code_refs(),
        skull_codegen_js::JS_NODE_HOST,
        &[],
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
fn js_str(s: &str) -> String {
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

/// Assemble the per-file node program: the shipped prelude over a host that
/// captures writes into `__buf`, the shared emitted defs (per-test isolated
/// closures concatenated, or one whole-file program when decorator applications
/// must run at load), then the harness running each test with its buffer reset
/// and comparing the captured stdout (trimmed) against the `output=` expectation
/// -- one `__SKULL__` line per test.
fn build_node_program(
    shared_defs: &str,
    tests: &[(String, String, Option<String>, bool)],
    has_code_refs: bool,
) -> String {
    let mut p = skull_codegen_js::assemble_prelude(shared_defs, has_code_refs, NODE_TEST_HOST, &[]);
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

/// Node's host with `write` captured per test. Runtime diagnostics
/// (`write_err`) stay out of the compared output.
const NODE_TEST_HOST: &str = r#"
var __buf = [];
var __rock_host = {
  write: function(s) { __buf.push(s); },
  write_err: function(s) { process.stderr.write(s); },
  env_var: function(n) { var v = process.env[n]; return v === undefined ? null : v; }
};
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn truncate_cuts_emitted_js_on_a_char_boundary() {
        let js = format!("{}\u{e9}{}", "a".repeat(3999), "b".repeat(10));
        let excerpt = crate::skull_backend::program_excerpt(&[&js]);
        assert!(excerpt.starts_with(&"a".repeat(3999)), "{}", &excerpt[..40]);
        assert!(excerpt.ends_with("more bytes)"), "{excerpt}");
    }
}
