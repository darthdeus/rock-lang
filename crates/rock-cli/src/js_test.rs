//! Emit JS test units, run them under Node, and report failures and skips.

use crate::Result;
use crate::test_protocol::{parse_skull_markers, reason_histogram};
use crate::test_unit::{UnitCheck, UnitFailure};
use bone_core::prelude::Instant;
use colored::Colorize;
use skull_mir::mir::FuncRef;

fn all_reasons(e: &rock_core::CompilerError) -> Vec<String> {
    std::iter::once(e.message.clone())
        .chain(e.siblings.iter().map(|s| s.message.clone()))
        .collect()
}

#[derive(Default)]
struct JsUnitRun {
    passed: usize,
    /// FAIL detail lines (throw under node, or a harness-invariant breach).
    fails: Vec<String>,
    /// All findings take part in the skip-reason histogram.
    skips: Vec<(String, Vec<String>)>,
    /// Tests `@test(backends = ...)` limits to other backends.
    other_backend: usize,
}

type JsUnitOutcome = std::result::Result<JsUnitRun, UnitFailure>;

pub fn js_test(paths: &[String]) -> Result<()> {
    let start = Instant::now();
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;
    let outcomes = crate::test_unit::run_units(&files, &package_dirs, None, js_build_unit);

    let mut tests_passed = 0;
    let mut tests_failed = 0;
    let mut tests_skipped = 0;
    // Tests `@test(backends = ...)` limits to other backends: never emitted,
    // never run, and not skips (the CI skip cap measures only JS gaps).
    let mut tests_other_backend = 0;
    let mut unit_failures = 0;
    let mut all_skip_reasons: Vec<String> = Vec::new();
    let file_count = outcomes.len();

    for (label, outcome) in outcomes {
        match crate::test_unit::unit_run(&label, outcome) {
            Some(JsUnitRun {
                passed,
                fails,
                skips,
                other_backend,
            }) => {
                tests_passed += passed;
                tests_failed += fails.len();
                tests_skipped += skips.len();
                tests_other_backend += other_backend;
                for (_, reasons) in &skips {
                    all_skip_reasons.extend(reasons.iter().cloned());
                }
                if passed + fails.len() + skips.len() + other_backend > 0 {
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
            None => unit_failures += 1,
        }
    }

    println!(
        "\njs backend: {}/{} @tests passed ({} failed, {} skipped, {} not for js) across {} files ({:.2}s)",
        tests_passed.to_string().green(),
        tests_passed + tests_failed + tests_skipped,
        if tests_failed == 0 {
            tests_failed.to_string().green()
        } else {
            tests_failed.to_string().red()
        },
        tests_skipped.to_string().yellow(),
        tests_other_backend,
        file_count,
        start.elapsed().as_secs_f64(),
    );
    if unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build/run (see FAILED lines above)",
            unit_failures.to_string().red()
        );
    }
    // Bucketed by exact message, count desc: the emitter work list.
    let skip_reasons = reason_histogram(all_skip_reasons.iter().map(|s| s.as_str()));
    if !skip_reasons.is_empty() {
        println!(
            "\n{}",
            "=== skip reasons (js) -- the emitter work list ===".bold()
        );
        let shown = skip_reasons.len().min(50);
        for (reason, count) in skip_reasons.iter().take(shown) {
            println!("  {:>5}  {}", count.to_string().yellow(), reason);
        }
        if skip_reasons.len() > shown {
            println!(
                "  {:>5}  ({} more distinct reasons)",
                "...",
                skip_reasons.len() - shown
            );
        }
    }
    if tests_failed > 0 || unit_failures > 0 {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

fn js_build_unit(unit: &str, is_package: bool) -> JsUnitOutcome {
    crate::test_unit::prepare_unit(unit)?;
    match crate::test_unit::check_unit(unit, is_package, true)? {
        UnitCheck::Checked { ws, tests, sources } => js_run_tests(&ws, &tests, &|e| {
            crate::skull_backend::render_project_errs(&sources, std::slice::from_ref(e))
        }),
        UnitCheck::ExpectedFailure(tests) => Ok(JsUnitRun {
            passed: tests,
            ..JsUnitRun::default()
        }),
    }
}

fn emit_test_program(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    skips: &mut Vec<(String, Vec<String>)>,
    render: &dyn Fn(&rock_core::CompilerError) -> String,
) -> std::result::Result<(String, bool, Vec<usize>), UnitFailure> {
    let emit_program = |indices: &[usize]| {
        let roots: Vec<_> = indices
            .iter()
            .map(|&i| (tests[i].0, format!("__t{i}")))
            .collect();
        skull_codegen_js::emit::emit_program_with_roots_artifact(ws, &roots)
            .map(|(js, artifact)| (js, artifact.has_escaping()))
    };

    let all: Vec<_> = (0..tests.len()).collect();
    let whole = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| emit_program(&all)));
    let whole_error = match whole {
        Ok(Ok((js, has_code_refs))) => return Ok((js, has_code_refs, all)),
        Ok(Err(error)) => Some(error),
        Err(_) => None,
    };

    // (test index, per-test isolated defs) for tests emittable in ISOLATION.
    let mut supported: Vec<(usize, String)> = Vec::new();
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

    let mut selected: Vec<_> = supported.iter().map(|(i, _)| *i).collect();
    selected.extend(deco_pending.iter().copied());
    selected.sort_unstable();
    if selected.is_empty() {
        return Ok((String::new(), false, selected));
    }

    let shared = whole_error
        .filter(|_| selected.len() == all.len())
        .map_or_else(|| emit_program(&selected), Err);
    match shared {
        Ok((js, has_code_refs)) => Ok((js, has_code_refs, selected)),
        Err(error) => {
            // A load-time effect can require JS features outside the supported
            // test bodies. Keep the isolated bodies that compiled successfully.
            for i in deco_pending {
                skips.push((tests[i].1.name.clone(), all_reasons(&error)));
            }
            let (selected, bodies): (Vec<_>, Vec<_>) = supported.into_iter().unzip();
            let js = bodies.concat();
            let has_code_refs = js.contains("__rock_code_slot_ref(");
            Ok((js, has_code_refs, selected))
        }
    }
}

/// Run JS-eligible tests in one Node batch.
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

    let (shared_defs, shared_has_code_refs, run_idxs) = if tests.is_empty() {
        (String::new(), false, Vec::new())
    } else {
        emit_test_program(ws, tests, &mut skips, render)?
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
            return Err(UnitFailure::new(
                "invalid JS emitted (js backend bug)",
                format!(
                    "stdout:\n{}\nstderr:\n{}\n\n--- emitted js (truncated) ---\n{}",
                    run.stdout,
                    run.stderr,
                    crate::skull_backend::program_excerpt(&[&program])
                ),
            ));
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
            return Err(UnitFailure::new(
                "harness invariant breach (js backend bug)",
                format!("VALUE line for `{name}` without a returns expectation"),
            ));
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

/// Stream a complete emitted program through Node and propagate its exit status.
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
        rock_engine_host::project::is_game_project(std::path::Path::new(dir)).unwrap_or(false);
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

/// True for the per-test reject a decorator-populated global read raises
/// (`emit.rs` `discover_globals`) -- the marker that a whole-file emission (which
/// runs the applications once) would lift the test.
fn is_decorator_populated_reject(msg: &str) -> bool {
    msg.contains("is populated by decorator application")
}

/// Wrap the definitions with the Node test host. Capture stdout per test
/// for `@test(output = ...)` and write one result marker per invocation.
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
            Some(s) => skull_codegen_js::emit::js_string_lit(s),
            None => "null".to_string(),
        };
        p.push_str(&format!(
            "  {{ f: {js_name}, name: {}, out: {out_js}, val: {is_value} }},\n",
            skull_codegen_js::emit::js_string_lit(name)
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
