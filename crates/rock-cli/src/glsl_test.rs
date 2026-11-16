//! `rock test <path> --backend glsl` -- execute the value-test class
//! (`@test(returns = <literal>)`) on the GPU via the GLSL emitter
//! (`skull-codegen-glsl`).
//!
//! Shares the backend-neutral half of the test driver with the luajit and node
//! paths: unit discovery (`collect_test_units`), the `@test` metadata
//! (`skull::module_tests_with_ids`), the `__SKULL__` protocol parser and the
//! ONE comparison site (`test_protocol::{parse_skull_markers, compare_value}`).
//! The host side never learns the lines came from a GPU -- G1 is a transport
//! for `__SKULL__` lines (the seam contract).
//!
//! GLSL cannot print, so ONLY the value class transfers. Two-tier skip:
//!   1. a test with no `returns=` is outside the value class -- reported as ONE
//!      collapsed count, never a histogram line (most of the suite is
//!      *permanently* ineligible, unlike JS).
//!   2. a test WITH `returns=` whose GLSL emission rejects is a real
//!      SKIP(reason); that per-profile histogram is the A1 fn-emission work
//!      list.
//!
//! The desktop profile (330 core) runs by default, with per-profile verdicts
//! (`name [330core]`). The heavy lifting -- creating a headless
//! GL context, compiling the shader, drawing to a 1x1 R32I/R32F target and
//! reading the pixel back -- lives in the standalone external GLSL runner binary
//! (spawned per unit+profile), so the `rock` compiler/LSP binary never links
//! SDL2. The runner is a workspace artifact, so a missing/broken runner is a
//! hard error (a gate that can't run must not read as green); only a genuine
//! no-display/driver environment (runner exit 2) may be skipped with the explicit
//! `--skip-gpu` option. Without it, every profile requires a display and GL
//! driver. A GL error mid-run is a loud FAIL, never a silent skip.

use crate::Result;
use crate::test_protocol::{compare_value, parse_skull_markers, reason_histogram};
use crate::test_unit::{UnitCheck, UnitFailure};
use bone_core::prelude::Instant;
use colored::Colorize;
use skull::ValueExpect;
use skull_codegen_glsl::{GlslProfile, ValueTestShaders, emit_value_test};
use skull_mir::mir::FuncRef;

/// GLSL is validated widely enough that f32 arithmetic and the R32F round-trip
/// introduce error well above the 1e-12 luajit/node tolerance -- ~1e-6 is f32
/// territory (24-bit mantissa). int/bool stay exact regardless.
const GLSL_FLOAT_REL_TOL: f64 = 1e-6;

/// The GL profile every test and shader validates against.
const PROFILE_NAME: &str = "330core";
const PROFILE: GlslProfile = GlslProfile::Core330;

/// Whole-run tallies of `rock test --backend glsl`.
struct GlslSuiteSummary {
    files: usize,
    /// `@test` fns with no `returns=` (outside the value class). One collapsed
    /// number, never a histogram.
    outside_value_class: usize,
    /// bone-rejected units (unexpected compile failure) / harness breaches.
    unit_failures: usize,
    passed: usize,
    /// FAIL detail lines (a GL compile/link/render error, or a value mismatch).
    failed: Vec<String>,
    /// SKIP reasons (emission rejected), bucketed count-desc.
    skip_reasons: Vec<(String, usize)>,
    /// False = no display / driver; GPU verdicts were explicitly skipped.
    gl_available: bool,
}

/// `rock test --backend glsl` entry: run, print per-unit lines + the
/// end-of-run summary, exit non-zero on any FAIL or unit failure.
pub(crate) fn glsl_test(paths: &[String], skip_gpu: bool) -> Result<()> {
    let start = Instant::now();
    let summary = glsl_test_run(paths, skip_gpu)?;

    println!(
        "\nglsl backend: {} @tests outside the value class (no returns=), across {} files ({:.2}s)",
        summary.outside_value_class.to_string().yellow(),
        summary.files,
        start.elapsed().as_secs_f64(),
    );
    if !summary.gl_available {
        println!(
            "  [{}] {} -- value verdicts SKIPPED (no GL context)",
            PROFILE_NAME.cyan(),
            "no display / driver".yellow()
        );
    } else {
        let total = summary.passed + summary.failed.len();
        println!(
            "  [{}] {}/{} value tests passed ({} failed)",
            PROFILE_NAME.cyan(),
            summary.passed.to_string().green(),
            total,
            if summary.failed.is_empty() {
                summary.failed.len().to_string().green()
            } else {
                summary.failed.len().to_string().red()
            },
        );
        for detail in &summary.failed {
            println!("    {} {}", "FAIL".red(), detail);
        }
        if !summary.skip_reasons.is_empty() {
            println!(
                "    {}",
                format!(
                    "=== [{PROFILE_NAME}] skip reasons (glsl) -- the A1 fn-emission work list ==="
                )
                .bold()
            );
            for (reason, count) in summary.skip_reasons.iter().take(50) {
                println!("      {:>5}  {}", count.to_string().yellow(), reason);
            }
        }
    }
    if summary.unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build (see FAILED lines above)",
            summary.unit_failures.to_string().red()
        );
    }
    if !summary.failed.is_empty() || summary.unit_failures > 0 {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Run the suite, printing per-unit lines as they complete.
fn glsl_test_run(paths: &[String], skip_gpu: bool) -> Result<GlslSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;

    // A missing runner is a hard error (workspace artifact -- build it). The
    // probe's exit 2 (no GL context) requires an explicit GPU opt-out; any
    // other probe anomaly is a hard error.
    let runner = locate_runner()?;
    let gl_available = probe_gl(&runner, PROFILE_NAME, skip_gpu)?;

    let mut summary = GlslSuiteSummary {
        files: 0,
        outside_value_class: 0,
        unit_failures: 0,
        passed: 0,
        failed: Vec::new(),
        skip_reasons: Vec::new(),
        gl_available,
    };
    let mut skip_reasons: Vec<String> = Vec::new();

    let outcomes = crate::test_unit::build_units(&files, &package_dirs, |unit, is_package| {
        build_unit(unit, is_package, &runner, gl_available)
    });

    for (label, outcome) in outcomes {
        summary.files += 1;
        match outcome {
            Ok(Ok(UnitRun {
                outside_value_class,
                passed,
                failed,
                skips,
            })) => {
                summary.outside_value_class += outside_value_class;
                summary.passed += passed;
                let any = passed + failed.len() + skips.len();
                if gl_available && any > 0 {
                    let status = if failed.is_empty() {
                        "ok".green()
                    } else {
                        "FAILED".red()
                    };
                    println!(
                        "{:.<52} [{}] {}  ({} pass, {} fail, {} skip)",
                        label,
                        PROFILE_NAME.cyan(),
                        status,
                        passed,
                        failed.len(),
                        skips.len(),
                    );
                }
                summary
                    .failed
                    .extend(failed.iter().map(|d| format!("{d} [{PROFILE_NAME}]")));
                skip_reasons.extend(skips.into_iter().map(|(_, reason)| reason));
            }
            Ok(Err(failure)) => {
                summary.unit_failures += 1;
                failure.print(&label);
            }
            Err(panic) => {
                summary.unit_failures += 1;
                crate::test_unit::print_unit_panic(&label, &*panic);
            }
        }
    }

    summary.skip_reasons = reason_histogram(skip_reasons.iter().map(|s| s.as_str()));
    Ok(summary)
}

/// One unit's value-test results.
#[derive(Default)]
struct UnitRun {
    outside_value_class: usize,
    passed: usize,
    failed: Vec<String>,
    /// (test name, reason) for value tests whose GLSL emission rejected.
    skips: Vec<(String, String)>,
}

/// One test unit (a file, or a package built as ONE workspace with its tests):
/// emit + run its value tests.
fn build_unit(
    unit: &str,
    is_package: bool,
    runner: &std::path::Path,
    gl_available: bool,
) -> std::result::Result<UnitRun, UnitFailure> {
    // Held through the tests: their shaders are emitted from the checked
    // workspace, which stays resident until the unit ends.
    let _slot = if is_package {
        Some(
            crate::compile_slots::acquire(unit).map_err(|error| UnitFailure {
                reason: "no compile slot".to_string(),
                rendered: format!("{error:#}"),
            })?,
        )
    } else {
        None
    };
    let checked = if is_package {
        crate::test_unit::check_package(unit, false)
    } else {
        crate::test_unit::check_file(unit, false)
    };
    Ok(match checked? {
        UnitCheck::Checked { ws, tests, .. } => run_tests(&ws, &tests, runner, gl_available),
        // Nothing to emit for a file whose rejection every test expected.
        UnitCheck::ExpectedFailure(_) => UnitRun::default(),
    })
}

/// Emit + GPU-run one unit's value tests.
fn run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    runner: &std::path::Path,
    gl_available: bool,
) -> UnitRun {
    let mut run = UnitRun::default();
    // (name, expected, shaders) for the tests that emitted.
    let mut runnable: Vec<(String, ValueExpect, ValueTestShaders)> = Vec::new();
    for (fr, t) in tests {
        // Value tests only (returns=). Non-value @tests are permanently outside
        // the class -- one collapsed count, never a histogram. So are tests
        // that `@test(backends = ...)` limits to other backends.
        let Some(expect) = t
            .returns
            .as_ref()
            .filter(|_| t.runs_on(bone_core::annotations::TestBackend::Glsl))
        else {
            run.outside_value_class += 1;
            continue;
        };
        // An int expectation outside i32 can't survive the R32I readback
        // (GLSL has no int64) -- a real SKIP, part of the work list.
        if let ValueExpect::Int(i) = expect {
            if *i < i32::MIN as i64 || *i > i32::MAX as i64 {
                run.skips.push((
                    t.name.clone(),
                    "value test: returns literal exceeds i32 range (GLSL has no int64)".to_string(),
                ));
                continue;
            }
        }
        match emit_value_test(ws, fr.module, &t.name, PROFILE) {
            Ok(shaders) => runnable.push((t.name.clone(), *expect, shaders)),
            Err(e) => run.skips.push((t.name.clone(), e.message.clone())),
        }
    }

    // No GL context -> the runnable ones are neither pass nor fail.
    if !gl_available || runnable.is_empty() {
        return run;
    }
    match run_batch(runner, PROFILE_NAME, &runnable) {
        Ok(markers) => {
            run.failed.extend(markers.fails);
            run.passed += markers.passed;
            let expects: std::collections::HashMap<&str, &ValueExpect> =
                runnable.iter().map(|(n, e, _)| (n.as_str(), e)).collect();
            for (name, val) in &markers.values {
                let Some(expect) = expects.get(name.as_str()) else {
                    run.failed.push(format!(
                        "harness breach: VALUE for `{name}` without an expectation"
                    ));
                    continue;
                };
                match compare_value(expect, val, GLSL_FLOAT_REL_TOL) {
                    Ok(()) => run.passed += 1,
                    Err(detail) => run.failed.push(format!("{name}: {detail}")),
                }
            }
        }
        Err(detail) => {
            // A runner-level failure (bad emitted GLSL, protocol breach) is a
            // loud FAIL for every runnable test, never a silent skip.
            for (name, _, _) in &runnable {
                run.failed.push(format!("{name}: runner error: {detail}"));
            }
        }
    }
    run
}

/// Spawn the runner for one unit+profile, feed the shaders as a JSON job array,
/// parse the `__SKULL__` lines back.
fn run_batch(
    runner: &std::path::Path,
    profile: &str,
    runnable: &[(String, ValueExpect, ValueTestShaders)],
) -> std::result::Result<crate::test_protocol::SkullMarkers, String> {
    let job = jobs_json(runnable);
    let (code, stdout, stderr) = spawn_runner(runner, profile, &job)?;
    if code == Some(2) {
        return Err(format!("runner reported no GL context: {}", stderr.trim()));
    }
    if code != Some(0) {
        return Err(format!("runner exited {code:?}: {}", stderr.trim()));
    }
    match parse_skull_markers(&stdout) {
        Some(m) if m.passed + m.fails.len() + m.values.len() == runnable.len() => Ok(m),
        _ => Err(format!(
            "invalid runner output (expected {} markers): stdout=<{}> stderr=<{}>",
            runnable.len(),
            stdout.trim(),
            stderr.trim()
        )),
    }
}

/// Serialize the runnable shaders to the runner's JSON job protocol.
fn jobs_json(runnable: &[(String, ValueExpect, ValueTestShaders)]) -> String {
    let items: Vec<serde_json::Value> = runnable
        .iter()
        .map(|(name, _, s)| {
            serde_json::json!({
                "name": name,
                "vertex": s.vertex,
                "fragment": s.fragment,
                "ret": s.ret.tag(),
            })
        })
        .collect();
    serde_json::Value::Array(items).to_string()
}

/// Probe: can the runner create a GL context for `profile` here? An empty job
/// exercises context creation and exits 0 on success, 2 on no-display/driver.
/// Only an explicit GPU opt-out permits skipping exit 2. A spawn failure or
/// any other exit code remains a hard error.
fn probe_gl(runner: &std::path::Path, profile: &str, skip_gpu: bool) -> Result<bool> {
    match spawn_runner(runner, profile, "[]") {
        Ok((Some(0), _, _)) => Ok(true),
        Ok((Some(2), _, stderr)) => {
            if !skip_gpu {
                crate::bail!(
                    "no display or GL driver for profile {profile}: {}. Install a display/GL driver or pass --skip-gpu to skip unavailable GPU profiles",
                    stderr.trim()
                );
            }
            log::warn!(
                target: "GLSL",
                "no GL context for profile {profile}: {}; verdicts skipped (--skip-gpu)",
                stderr.trim()
            );
            Ok(false)
        }
        Ok((code, _, stderr)) => crate::bail!(
            "stone-glsl-runner probe for profile {profile} exited {code:?} (expected 0, or 2 for no GL context): {}",
            stderr.trim()
        ),
        Err(e) => crate::bail!("cannot spawn stone-glsl-runner ({}): {e}", runner.display()),
    }
}

/// Spawn the runner with the game's library-search env (so SDL finds libGL the
/// same way the luajit game does) and pipe the job JSON to stdin.
fn spawn_runner(
    runner: &std::path::Path,
    profile: &str,
    job: &str,
) -> std::result::Result<(Option<i32>, String, String), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(runner);
    rock_host::compiler_thp::configure_runtime_command(&mut cmd);
    cmd.arg("--profile").arg(profile);
    cmd = crate::runtime_process::configure(cmd, crate::runtime_process::RuntimeProcess::Native);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(job.as_bytes())
        .map_err(|e| format!("write stdin: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

// --- Compile-validate registered render shaders on real GL contexts ---
// The value-test corpus above proves the EMITTER on desktop GL; this proves
// the shaders games actually register.

/// One profile's tallies over a `shader-validate` run.
pub struct ValidateProfileSummary {
    /// `"330core"`.
    pub profile: &'static str,
    pub passed: usize,
    /// FAIL detail lines (driver compile/link log, or a harness breach).
    pub failed: Vec<String>,
    /// False = no GL context for this profile (explicitly skipped).
    pub gl_available: bool,
}

/// Whole-run tallies for `rock shader-validate` -- the testable core.
pub struct ValidateSummary {
    /// (project dir, registered shader-pair count).
    pub projects: Vec<(String, usize)>,
    pub per_profile: Vec<ValidateProfileSummary>,
}

/// `rock shader-validate <project>...`: print verdicts, exit non-zero on any
/// FAIL; missing GL profiles require the explicit GPU opt-out.
pub(crate) fn shader_validate(dirs: &[String], skip_gpu: bool) -> Result<()> {
    if dirs.is_empty() {
        crate::bail!(
            "rock shader-validate: usage: rock shader-validate <project-dir> [project-dir...]"
        );
    }
    let start = Instant::now();
    let summary = shader_validate_run(dirs, true, skip_gpu)?;

    let shader_total: usize = summary.projects.iter().map(|(_, n)| n).sum();
    println!(
        "\nshader-validate: {} registered pair(s) across {} project(s) ({:.2}s)",
        shader_total,
        summary.projects.len(),
        start.elapsed().as_secs_f64(),
    );
    let mut any_failed = false;
    for p in &summary.per_profile {
        if !p.gl_available {
            println!(
                "  [{}] {} -- verdicts SKIPPED (no GL context)",
                p.profile.cyan(),
                "no display / driver".yellow()
            );
            continue;
        }
        println!(
            "  [{}] {}/{} compiled+linked",
            p.profile.cyan(),
            p.passed.to_string().green(),
            p.passed + p.failed.len(),
        );
        for detail in &p.failed {
            any_failed = true;
            println!("    {} {}", "FAIL".red(), detail);
        }
    }
    if any_failed {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

/// Run the validation; public for the gate test (`print = false`). Hard errors
/// (missing runner, a project that fails to build, a pair that fails to EMIT)
/// are `Err` -- the same pairs emit inside every game build, so an emit failure
/// here is a regression or a profile-specific emitter gap, never a skip.
pub fn shader_validate_run(
    dirs: &[String],
    print: bool,
    skip_gpu: bool,
) -> Result<ValidateSummary> {
    let runner = locate_runner()?;
    let gl_available = probe_gl(&runner, PROFILE_NAME, skip_gpu)?;
    let mut profile_summary = ValidateProfileSummary {
        profile: PROFILE_NAME,
        passed: 0,
        failed: Vec::new(),
        gl_available,
    };
    let mut projects = Vec::new();

    for dir in dirs {
        // Held while this project's shaders are emitted and linked from its
        // resident workspace.
        let _slot = crate::compile_slots::acquire(dir)?;
        let ws = validate_workspace(dir)?;
        let pairs = skull_mir::lower::collect_shader_pairs(&ws)
            .map_err(|e| anyhow::anyhow!("{dir}: collect_shader_pairs: {}", e.message))?;
        projects.push((dir.clone(), pairs.len()));
        let programs: Vec<skull::shader_artifact::ShaderProgram> = pairs
            .iter()
            .map(|pair| skull::shader_artifact::ShaderProgram {
                vert: pair.vert,
                frag: pair.frag,
            })
            .collect();
        let roots: Vec<FuncRef> = programs
            .iter()
            .flat_map(|program| {
                program
                    .vert
                    .into_iter()
                    .chain(std::iter::once(program.frag))
            })
            .collect();
        let graph = skull::instance_graph::build(&ws, roots, false);
        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(&ws)
            .sharing(std::sync::Arc::clone(graph.interner()));

        if !gl_available {
            continue;
        }
        let view = skull::shader_artifact::realize_shader_with_context(
            &ws,
            &graph,
            &skull::shader_artifact::ShaderArtifactRequest {
                spec: skull::target_projection::TargetSpec {
                    shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(PROFILE)),
                    ..skull::target_projection::LUAJIT_DESKTOP
                },
                programs: &programs,
            },
            &shader_scx,
        )
        .ok_or_else(|| anyhow::anyhow!("{dir}: GLSL shader view is unavailable"))?;
        if view.program_views.len() != pairs.len() {
            anyhow::bail!("{dir}: GLSL shader view/collection denominator differs");
        }
        let mut jobs: Vec<(String, String, String)> = Vec::with_capacity(pairs.len());
        for (pair, program_view) in pairs.iter().zip(&view.program_views) {
            let emission =
                skull_codegen_glsl::emit_shader_with_view(&ws, &shader_scx, program_view, PROFILE)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "{dir}: `{}` failed to emit for {PROFILE_NAME}: {}",
                            pair.key,
                            e.message
                        )
                    })?;
            let (vertex, fragment) = split_stages(&emission.source);
            jobs.push((format!("{dir}:{}", pair.key), vertex, fragment));
        }
        if jobs.is_empty() {
            continue;
        }
        let (passed, failed) = compile_batch(&runner, PROFILE_NAME, &jobs)?;
        if print {
            let status = if failed.is_empty() {
                "ok".green()
            } else {
                "FAILED".red()
            };
            println!(
                "{:.<52} [{}] {}  ({} pass, {} fail)",
                dir,
                PROFILE_NAME.cyan(),
                status,
                passed,
                failed.len(),
            );
        }
        profile_summary.passed += passed;
        profile_summary.failed.extend(failed);
    }
    Ok(ValidateSummary {
        projects,
        per_profile: vec![profile_summary],
    })
}

/// Build a project directory into ONE type-checked workspace, tests included
/// (sims may register their own `rock_shader` pairs -- validate those too).
pub(crate) fn validate_workspace(dir: &str) -> Result<bone_semantic::inference::Workspace> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec)
        .map_err(|e| anyhow::anyhow!("{dir}: cannot load project: {e}"))?;
    validate_sources(sources)
}

fn validate_sources(
    sources: rock_core::SourceFiles,
) -> Result<bone_semantic::inference::Workspace> {
    crate::skull_backend::check_project(&sources)
        .map(|(ws, _entry)| ws)
        .map_err(|errs| {
            anyhow::anyhow!(
                "{}",
                crate::skull_backend::render_project_errs(&sources, &errs)
            )
        })
}

/// Split a single-file `#ifdef VERTEX` / `#ifdef PIXEL` shader into the two
/// stage sources EXACTLY the way the engine loader does
/// (`engine-rs/src/shader.rs`): inject `#define VERTEX` / `#define PIXEL` on the
/// line after `#version`. Validating anything else would validate a shader the
/// engine never compiles.
fn split_stages(combined: &str) -> (String, String) {
    let insert_point = combined
        .find("#version")
        .and_then(|vp| combined[vp..].find('\n').map(|p| vp + p + 1))
        .unwrap_or(0);
    let (prefix, suffix) = combined.split_at(insert_point);
    (
        format!("{prefix}#define VERTEX\n{suffix}"),
        format!("{prefix}#define PIXEL\n{suffix}"),
    )
}

/// One runner invocation with compile-only jobs; returns (passed, FAIL details).
/// Marker-count and `compiled` payloads are verified -- a protocol drift is a
/// loud error, never a silent pass.
fn compile_batch(
    runner: &std::path::Path,
    profile: &str,
    jobs: &[(String, String, String)],
) -> Result<(usize, Vec<String>)> {
    let items: Vec<serde_json::Value> = jobs
        .iter()
        .map(|(name, vertex, fragment)| {
            serde_json::json!({ "name": name, "vertex": vertex, "fragment": fragment, "ret": "compile" })
        })
        .collect();
    let payload = serde_json::Value::Array(items).to_string();
    let (code, stdout, stderr) = spawn_runner(runner, profile, &payload)
        .map_err(|e| anyhow::anyhow!("runner [{profile}]: {e}"))?;
    if code != Some(0) {
        crate::bail!(
            "runner [{profile}] exited {code:?} mid-run: {}",
            stderr.trim()
        );
    }
    let markers = parse_skull_markers(&stdout)
        .filter(|m| m.passed + m.fails.len() + m.values.len() == jobs.len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid runner output (expected {} markers): stdout=<{}> stderr=<{}>",
                jobs.len(),
                stdout.trim(),
                stderr.trim()
            )
        })?;
    let mut passed = 0usize;
    let mut failed: Vec<String> = markers
        .fails
        .iter()
        .map(|d| format!("{d} [{profile}]"))
        .collect();
    for (name, val) in &markers.values {
        if val == "compiled" {
            passed += 1;
        } else {
            failed.push(format!(
                "harness breach: `{name}` returned `{val}`, expected `compiled` [{profile}]"
            ));
        }
    }
    Ok((passed, failed))
}

/// Locate the external GLSL runner binary: `ROCK_GLSL_RUNNER` override, then a
/// sibling of the running `rock` binary (production + release layout), then one
/// level up (cargo test binaries live in `target/<profile>/deps/`). Missing is
/// a hard error: the runner is a workspace artifact, and a gate whose executor
/// is absent must not read as green.
fn locate_runner() -> Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ROCK_GLSL_RUNNER") {
        let p = std::path::PathBuf::from(p);
        if !p.exists() {
            crate::bail!(
                "ROCK_GLSL_RUNNER points at a missing stone-glsl-runner: {}",
                p.display()
            );
        }
        return Ok(p);
    }
    let name = if cfg!(windows) {
        "stone-glsl-runner.exe"
    } else {
        "stone-glsl-runner"
    };
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("exe has no parent dir"))?;
    for cand in [
        dir.join(name),
        dir.parent().map(|d| d.join(name)).unwrap_or_default(),
    ] {
        if cand.exists() {
            return Ok(cand);
        }
    }
    crate::bail!(
        "stone-glsl-runner not found next to {} -- build it (`cargo build -p stone-glsl-runner`) or set ROCK_GLSL_RUNNER",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_tests_wait_for_a_compile_slot() {
        let package = tempfile::tempdir().expect("package");
        std::fs::write(
            package.path().join("stick.toml"),
            "[package]\nname = \"slot-probe\"\ntype = \"lib\"\n",
        )
        .unwrap();
        std::fs::write(
            package.path().join("probe.rock"),
            "@test fn probe() -> Int = 1\n",
        )
        .unwrap();
        let dir = package.path().to_str().unwrap().to_owned();
        // No GL profile is available, so no runner is spawned.
        crate::compile_slots::assert_compile_waits_for_slot(move || {
            let _ = super::build_unit(&dir, true, std::path::Path::new("unused-runner"), false);
        });
    }
}
